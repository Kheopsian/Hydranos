# Changelog

All notable changes to Hydra are documented here. This project follows
[semantic versioning](https://semver.org).

## v3.98.1 - 2026-08-20

### Fixed
- The category rollups no longer read the whole database. `GROUP BY category`
  had no index, so it scanned every torrents row -- blob included -- which at
  195k torrents is 4 GB and ~19 s, spent holding the store's single connection:
  the tracker list, Add torrent and everything else queued behind it. A
  covering index on `(category, session, save_path)` brings the same query to
  ~0.2 s. The index is built once, at the first boot on this version (~1 min on
  a large database).
- Page load no longer fetches the category rollups when the categories screen
  is not open; the poll loop already did that, per tab.

## v3.98.0 - 2026-08-20

### Changed

- **The peer_id fingerprint is derived from the version instead of being kept
  in step with it by hand, and encodes in base62.** The prefix has four
  characters between `-HY` and the closing dash, and the old encoding spent
  them on decimal digits: one for the major, two for the minor, one for the
  patch. That overflows into a ninth byte at 3.100.0, which the length guard
  truncated back to eight, dropping the closing dash and leaving a malformed
  prefix that trackers accept without complaint. At roughly four minor bumps a
  day, 3.100.0 was days away. The four characters are now base62 -- one for the
  major, two for the minor, one for the patch -- which holds a minor up to 3843
  instead of 99, and the value is computed from `Version` at package init so
  there is no second copy to drift. A version that does not fit panics at
  startup and under test rather than being silently truncated onto the wire.
  `3.98.0` is `-HY31a0-`.

  This changes the prefix for a given version: 3.97.0 was `-HY3970-` and would
  now be `-HY31Z0-`. Nothing on the protocol depends on it, the peer_id being
  regenerated per session, and no third-party client reads it correctly either
  way -- a generic Azureus-style parser takes four independent single-character
  fields, so libtorrent has always rendered 3.97.0 as "HY 3.9.7.0". Base62 is
  what Transmission writes and what its `clients.cc` decodes, and it agrees
  with the base36 libtorrent emits on every value below 36.

## v3.97.0 - 2026-08-20

### Added

- **The agent token can be set from the config file or the environment.** `--agent-token` was the only way to give a node's gRPC data-plane its shared secret, which meant putting it in the command line of every agent -- visible in `ps`, and in the Kubernetes manifest or compose file that spells the command out. It now comes from `[daemon] agent_token` or `$HYDRA_AGENT_TOKEN` as well, so the token can travel as a mounted config or a secret reference like every other credential. Precedence runs `--agent-token`, then `$HYDRA_AGENT_TOKEN`, then `[daemon] agent_token`; an empty or absent environment variable falls through to the config rather than silently disabling authentication, and `--agent-token=""` remains the explicit way to turn it off. The value is trimmed, because a secret arriving from an env file or a mounted volume usually carries a trailing newline and a token off by one invisible byte fails with nothing on either side to explain why. Where the token came from is logged; the token itself is not. `agentprobe` reads the same variable when `-token` is not given.

## v3.96.3 - 2026-08-20

### Fixed

- **The Jobs table was capped at 600px.** It was wrapped in the add-form
  container, which is the right width for a form and the wrong one for a table
  whose two interesting columns are a release name and a filesystem path: both
  wrapped into narrow ribbons while two thirds of the screen sat empty. The
  table now sits straight in the section, with its controls moved up beside the
  title. Its columns are fixed rather than automatic, so they no longer resize
  on every two-second tick as the byte counts grow a digit.

## v3.96.2 - 2026-08-20

### Changed

- **A job names the torrent it is acting on.** The Jobs list identified a
  torrent by the first twelve characters of its hash, which is unreadable and,
  since the torrent filters do not search by hash, not something that could be
  pasted anywhere useful either. The name is now captured into the job at
  submission rather than resolved when the list is drawn: a job outlives the
  torrent it acted on, so a lookup against currently loaded rows would leave
  exactly the finished and failed entries blank. Jobs created before this fall
  back to the last segment of the destination path, which for a move is the
  release folder. The hash is still shown in full, monospaced and select-all,
  because it is what gets pasted into an API call or grepped out of a log.

## v3.96.1 - 2026-08-20

### Added

- **A Jobs tab.** The move endpoints told the operator to follow the work in
  Jobs and there was no such place. There is now: state, progress, destination,
  and a cancel button while a job is still running. It polls only while it is
  on screen, because a table nobody is looking at does not need refreshing.

### Fixed

- **A move that came back EXDEV gave up instead of falling back to a copy.**
  `stat` cannot promise that a rename will succeed - a source that is itself a
  mount root will not be renamed at all, and overlay and network filesystems
  have rules of their own - so the rename is now the probe and the copy is the
  fallback. On EXDEV the move re-checks the things a copy needs and a rename
  did not: free space, and consent to break hardlinks, which was never asked
  for because the operator had been told this would be a rename. The torrent is
  restarted before the copy begins rather than held stopped for its duration.
- **A torrent whose content root was a mount point would have been relocated
  and then deleted.** Pointing the first real move at a torrent seeding
  directly from a bind-mounted share would have moved the entire volume and
  removed the original. `Inspect` now refuses any source that is a mount point,
  in the preview as well as at submission.

## v3.95.2 - 2026-08-20

### Fixed

- **The category picker did not open on race rows, and the tag picker threw.**
  The previous release meant to drop the hoard-only guard from the category
  submenu. The guard is two lines that appear verbatim in both submenu
  builders, and the edit landed on the first one it found, which was the tag
  picker: category items were shown on race rows but clicking them returned
  immediately, while the tag picker lost the binding the rest of its body reads
  and threw instead of opening. Both are now edited from anchors that include
  the function signature, so each edit can only match one place. Tags stay
  hoard-only, which is why that guard belongs where it is.

## v3.95.1 - 2026-08-20

### Added

- **Change category is offered on race rows, and breaking hardlinks is asked
  about once.** The context menu hid both category items unless the selection
  contained a hoard row, because the endpoint behind them was hoard-only. It is
  not any more: setting a category on a race torrent is precisely how that
  torrent is handed to the hoard. Each torrent is now addressed on the engine
  that actually holds it instead of always on `/api/hoard`. A move that would
  break hardlinks comes back as 409 with a reason rather than a generic
  failure; the browser collects those refusals across the whole selection, asks
  once with the file count and the total size, and retries the ones the
  operator accepted. A queued move answers 202, so a right-click that starts an
  hour of copying does not look like a right-click that did nothing. Recheck
  and tags stay hoard-only: those really are hoard-only operations.

## v3.95.0 - 2026-08-20

### Added

- **Changing a torrent's category relocates its payload data.** On one
  filesystem that is a rename and finishes in milliseconds; across filesystems
  it is a copy that runs for as long as it runs. Both go through the job runner
  rather than one being special-cased into the request, so there is a single
  answer to what is happening to a torrent regardless of which case it fell
  into. An engine handover and a data move compose: a race torrent given a
  hoard-mode category whose `save_path` is elsewhere is handed over first, then
  moved. Refusals are machine-readable because the operator has to answer them:
  breaking hardlinks returns 409 with the count, the bytes, example filenames
  and `retry_with: allow_breaking_hardlinks`; not enough space returns 507 with
  what was needed and what was free. A torrent whose data sits loose in a
  shared category directory is refused outright, because moving it would move
  every other torrent in that directory with it. `GET move-preview` answers the
  same questions without touching anything.
- **Durable background jobs.** Relocating a payload across filesystems takes
  minutes to hours, so it cannot live inside a request: the caller would hold a
  connection open for the duration, a restart would lose track of what was
  half-done, and nothing else could ask what is running. The jobs table and
  runner are deliberately general - a move is the first type, not the only one.
  The move is split in two: `Inspect` only looks, walking the payload and
  counting multiply-linked files, and its findings are what a caller turns into
  a prompt; `Execute` acts, and only after being told explicitly that those
  findings are acceptable, a permission captured in the job's params so a
  resumed job never silently re-answers it. Ordering is the whole design. A
  cross-filesystem move copies into a staging directory beside the target while
  the torrent keeps seeding from the source, verifies what landed, stops the
  torrent, swaps, repoints the engine, restarts it, and only then removes the
  old copy. The source is never removed before the destination is verified and
  in place, so an interruption leaves the payload where the torrent is still
  seeding from it, plus a staging directory the next attempt reuses. Refusals -
  not enough free space, a target inside the source, a target that already
  exists - happen up front, because discovering any of them two hours into
  copying a 400 GB release is the worst possible moment to find out.
- **`[daemon] move_max_mb_per_sec` caps move throughput, at 200 MB/s by
  default.** A cross-filesystem move reads and writes the same disks the
  torrents are served from, and left uncapped it takes whatever the array can
  give while the seeding is what gives way. 200 MB/s sits clearly below what
  the array sustains and still finishes 100 GB in roughly eight minutes. The
  setting distinguishes unset from zero: unset gets the default, an explicit
  zero means no cap at all. Only the copy path is affected, a same-filesystem
  move being a rename that moves no bytes.

### Fixed

- **A job was cancelled the instant the request that created it returned.**
  Jobs were started with the HTTP request's context. The manager now holds the
  daemon's context and `Submit` takes none at all: a job outliving its request
  is the definition of a job, so the API must not offer a way to say otherwise.
  The safety ordering held while this was broken - the source was untouched,
  the target absent, and the torrent still seeding.

## v3.94.0 - 2026-08-20

### Added

- **Per-torrent state moved out of the resume directory and into SQLite.** That
  directory was not slow to read: at production scale, reading the records is
  about 5% of a cold start against 94% spent re-parsing `.torrent` files. It
  was expensive to write. `save_all_resume` rewrote every torrent on every
  five-minute tick whether or not it had changed - roughly 200k file
  create/write/rename cycles every 300 seconds, about 666 per second, on a
  machine that was otherwise idle. Each few-hundred-byte record occupies a
  whole filesystem block, so a sweep dirtied around 780 MiB of copy-on-write
  blocks that the hourly snapshots then pinned, which made the resume directory
  the dominant source of snapshot growth on the pool. A directory of files also
  has no transaction: durability was hand-rolled with `.tmp` plus rename,
  deletions leaked orphans, and a torrent's identity in the Go-side store could
  silently disagree with its progression. Typhon now writes only the rows whose
  fingerprint moved - the hot set rather than the total - in a single
  transaction, to one database per engine. The legacy directory is imported
  once on first start and deliberately left on disk, so rolling back is just
  running an older build. `TYPHON_STATE_DB=0` falls back to the old scheme
  entirely, and `TYPHON_RESUME_JSON=1` keeps both in step.
- **A torrent changes engine by changing its category.** A category already
  carries the engine its torrents belong in, so setting a torrent's category to
  one whose mode differs from its current engine now performs the handover: the
  target adopts the exported record - bitfield, counters, edited trackers,
  added and completed times - and only then does the source let go, so an
  interruption leaves the torrent in both engines rather than in neither.
  Because the record that crosses is the same one a restart would read back,
  the torrent does not re-check a byte. Payload files are deliberately not
  relocated here: the adopting engine seeds the data where it already is.
  Moving a torrent to or from a remote agent is refused outright rather than
  half-performed.

## v3.93.1 - 2026-08-19

### Fixed

- **The first announce after a start left directly, ignoring `announce_proxy`.**
  The race engine built its tracker announcer from a listen port alone, so every
  other announce-egress setting defaulted to nothing - `announce_proxy` included.
  A relay setup whose hoard announced through the proxy therefore had its race
  announces go straight out, handing the tracker this host's own address. It
  showed at startup because the seed keepalive fires on its first tick after a
  restart, when no torrent has a recent announce yet; once the proxied loops had
  run, nothing was due again for 25 minutes and the path looked clean. The
  tracker was left holding a second seeding location per restart, which on a
  tracker that caps locations makes later announces fail outright. `announce_ip`
  and `enable_ipv6` were dropped on the same floor and are now carried too.
  The built-in network check could not have caught this: it runs on demand,
  always in the steady state where the path really is correct.

- **Race announces minted a new peer_id every 30 seconds.** The keepalive built
  a fresh announcer per tick, and building one generates an identity, so the
  tracker saw a stream of distinct peers claiming the same port rather than one
  peer refreshing. The announcer is now built once and rebuilt only when the
  bound port actually moves.

## v3.93.0 - 2026-08-19

### Fixed

- **A resume record could be left truncated by a crash.** Saving one wrote
  straight to its final path, so an interruption part-way through - a crash, an
  OOM kill, a full disk - left a half-written or zero-byte file. Loading can
  only warn and skip such a record, so the torrent silently lost its resume
  state at the next start. Two records were sitting in exactly that state in a
  195k-torrent library, months old and unnoticed, because one warning line in a
  busy log is invisible. Saves now write a sibling temporary file and rename it
  into place, which is atomic: a reader sees the old record or the new one,
  never a partial one.

### Added

- **Startup now reports where its time actually goes.** Reloading the library
  does two very different things: it reads the resume records, then it re-parses
  the .torrent file each record points at. The second is where the piece hashes
  live and is far the larger of the two, but with a single duration in the log
  there was no way to tell them apart, and a slow start got blamed on whichever
  half was easier to imagine. The engine now logs both, with the record count
  and the bytes of .torrent re-parsed.

### Changed

- **A temporary resume file left behind by an interrupted save is now swept.**
  Nothing ever reads one, so without this they would accumulate one per crash
  and never leave.

## v3.92.2 - 2026-08-19

### Added
- **Edit a torrent's trackers from its detail panel.** Add, remove or rename them: the whole list is edited as text, one URL per line, a blank line starting a new tier. Tiers are tried in order, so they are kept rather than flattened. The editor opens from the card header rather than the table, which refreshes on a timer, and that table stops redrawing while the editor is open. The change applies from the next announce and is written to disk, so it survives a restart.
- **`GET` and `POST /api/torrents/:hash/trackers`**, with `add`, `remove`, `replace` and `set` operations. Adding a tracker that is already there reports that nothing changed, so a bulk pass can skip the work; removing the last URL of a tier drops the tier; replacing keeps the URL's position, which is what makes a domain migration safe across many torrents.
- **The tracker panel shows when each tracker last answered**, next to when the next announce is due. Only successful announces move it, so a failing tracker shows how long since it actually worked rather than since we last tried.

### Fixed
- **`POST /api/torrents/:hash/add-tracker` did nothing and answered 200.** It called a no-op on both engines, so every caller since believed it had added a tracker.
- **The tracker list reported only the first URL of each tier**, hiding every fallback a tier held.
- **A tracker edit did not survive a restart.** The engine reloads from its resume records, which carried no tracker URLs, so the list fell back to what the .torrent file said. It is now part of the resume record and wins over the file. The stored .torrent is rewritten as well, with the info dict copied through byte for byte: editing trackers cannot change a torrent's infohash.
- **The race panel showed no announce timings at all**, and its "next announce" read "now" forever. Typhon's internal announce loop is disabled for both engines, so the Go announce cycle is the only source of tracker state, and the race half of that feed had never been connected.
- **The Exit IP block stretched the header.** It was the only header stat with no width limit and the only one carrying horizontal padding, so it ran to roughly 300 pixels and opened a 44 pixel gutter around itself against 24 elsewhere. One address per line now, in a rounded box sized to its content, with the full pair on the tooltip and the incognito masking applied there too.
- **The peer fingerprint still advertised 3.88.0** after three version bumps.

## v3.91.0 - 2026-08-19

### Fixed
- **A missing config file at an explicit `--config` path was fatal.** Only a config Hydra was left to find on its own was ever seeded; passing `--config /config/default.toml` at a path that did not exist logged "Failed to load config" and exited, so a container starting on an empty volume died before it could write one, and died again on every restart. That path is now seeded from the embedded template like any other, in every mode -- `--agent-only` and `--front-only` included -- with `data_dir` pointing at the config's own directory when the path is absolute, matching what `entrypoint.sh` writes on a first run. Deployments that bypass the entrypoint (Kubernetes, in particular) now come up on an empty volume instead of crash-looping. The seed is written to a temporary file and renamed into place, so the agent and the front end of one pod starting together never read a half-written config, and a config seeded as root is handed to `PUID`/`PGID` the way the entrypoint would have. A path that exists but cannot be read, and a path that is a directory, are reported rather than written over, and seeding an explicit path is logged as a warning: a typo in `--config` lands there too, and a fresh config means an instance that knows nothing about the data it was meant to pick up.

## v3.88.0 - 2026-08-18

### Fixed
- **hydra.log grew forever.** Every hub entry was mirrored to a file nothing ever truncated, and the engines log a line per inbound peer connection, so the production instance reached 41 GB on its cache SSD. The mirror now rotates at 128 MiB and keeps five generations, capping it at 640 MiB whatever the traffic.
- **Engine log lines were all filed as INFO,** warnings and errors included, so the Logs tab level filter did nothing for engine sources. The level is written with ANSI colour codes wrapped around it, which the parser did not expect.

## v3.87.0 - 2026-08-18

### Fixed
- **Stopping a torrent left its DHT lookup running forever.** The `get_peers` task only ever exited on the removal flag, and only when the stream happened to yield a peer, so stopped torrents kept querying the DHT for the life of the process. Tasks are now cancelled on stop and on remove, and re-armed on start.
- **The DHT peer lookup had no ceiling on work in flight.** Requests were pushed into an unbounded queue fed by an unbounded channel, and each answer enqueued up to eight more nodes, so a large torrent set grew the heap without bound — the hoard engine reached 28.9 GB before the kernel killed it. Both ends are now capped.

## v3.86.1 - 2026-08-18

### Fixed
- **Dropdowns in the settings looked like plain text fields.** The shared input style used the `background` shorthand, which resets `background-image` and so erased the chevron drawn for every `<select>`. Both the VPN interface and the engine that takes the forwarded port read as a value someone else had set rather than something to click.

## v3.86.0 - 2026-08-18

### Added
- **Choose which engine takes the forwarded port from gluetun.** A provider forwards one port, so one engine gets it and the other keeps its own; until now that was always the hoard, with no way to say otherwise. Hoard seeds around the clock, so being reachable pays off continuously, while race needs peers quickly on a fresh torrent. Turning the choice around moves the setting off the engine that had it in the same save, so the two can never end up bound to the same port.

## v3.85.0 - 2026-08-16

### Added
- **Reset every setting to defaults**, from the button at the right of the settings toolbar. It rebuilds the config from the one a fresh install ships, keeping only the login, the API key and the data directory, since losing those cannot be undone from the UI. The previous config is copied next to it first, under its own name so the next save cannot overwrite it.

## v3.84.2 - 2026-08-16

### Changed
- **The unsaved-changes dialog offers "Save and restart" when the edits need one.** Saving and leaving dropped the user on another page with a restart still owed and the notice about it back on the page they had just left. Settings that apply live still offer "Save and leave".

## v3.84.1 - 2026-08-16

### Fixed
- **The unsaved-changes prompt never appeared.** It used a pluralisation helper that is not a global, so building the dialog threw before it was shown: the page switch was blocked and nothing explained why.

## v3.84.0 - 2026-08-16

### Added
- **Leaving the settings page with unsaved changes now asks first.** Switching to another page discarded them silently, with nothing to show they had existed. The prompt offers three explicit ways out, since two of them lose work: save and leave, discard and leave, or stay. Closing the browser tab warns as well.

## v3.83.4 - 2026-08-16

### Fixed
- **The restart notice sits under the save button instead of at the top of the card.** Saving happens at the end of a long panel, so a notice above everything meant scrolling back up to learn a restart was owed.

## v3.83.3 - 2026-08-16

### Fixed
- **A pending restart no longer disappears when you move around.** Re-rendering the settings page cleared the notice, so changing tab after a save left the daemon running settings nobody could see any more. It is remembered until the restart happens.
- **Every tab announces the restart in the same place.** The Network tab put it at the bottom of its own panel while the others used the banner at the top; there is one banner now, kept in view at the bottom of the card.

## v3.83.2 - 2026-08-16

### Fixed
- **Clicking the address dropped the IPv6 one.** Two pieces of code wrote the header address and only the polling one knew about IPv6, so a manual refresh replaced both addresses with the v4. There is a single renderer now.

## v3.83.1 - 2026-08-16

### Fixed
- **v3.83.0 did not build.** Its commit carried unrelated engine changes that reference a symbol not yet published, so the release tag failed to compile. Only the inbound counter remains.

## v3.83.0 - 2026-08-16

### Added
- **Reachability is now proven by the peers that reach you.** The engine counts connections opened to it, excluding our own addresses so a probe cannot validate itself, and one stranger getting through settles the question in every mode, including inside a tunnel where a self-sent probe is structurally blind. No third-party port checker, nothing about your address or port handed to anyone.

## v3.82.0 - 2026-08-16

### Fixed
- **The reachability dot no longer reports a working VPN as unreachable.** The probe leaves through your own tunnel and comes back to the provider's address, which is not obliged to return it to its own client: measured on ProtonVPN, a port that peers reach perfectly well answers nothing from inside the tunnel. A refusal is now only reported as closed when the probe genuinely came from outside, through a proxy; otherwise the state is unknown and says why, mentioning gluetun's forwarded port when there is one.

## v3.81.3 - 2026-08-16

### Changed
- **The reachability dots are labelled.** Hoard and Race are written beside them, and the tooltip now covers the whole row rather than the dot alone.

## v3.81.2 - 2026-08-16

### Changed
- **Hoard sits above race in the header, and the dots are a little larger.**

## v3.81.1 - 2026-08-16

### Changed
- **The reachability dots lost their letters and now sit one above the other.** With R and H inside them they read as lottery balls; the position carries the engine and the tooltip names it.

## v3.81.0 - 2026-08-16

### Fixed
- **IPv6 turned on but unavailable is now visible.** The setting only makes Hydra listen on IPv6 if the host has an address; on a host without one, ticking it changed nothing and looked exactly like it had worked. The header now shows "IPv6 unavailable" where the second address would be, with the reason on hover.

## v3.80.0 - 2026-08-16

### Added
- **A gluetun mode in the VPN setup.** Tick it and the hoard engine asks gluetun for the port the provider forwarded, binds that, and follows it when the lease rotates. Crucially it does not announce before it has one: publishing the configured port first hands every tracker an address that answers nobody for a whole announce cycle, so announces and peer dials are held from boot until the port is bound. A tunnel that never yields a port releases the hold after ten minutes rather than staying silent forever, since a wrong port is visible and fixable while silence is not.

## v3.79.0 - 2026-08-16

### Changed
- **The header dot now reports whether peers can reach you, not whether you have peers.** It was lit by a peer count, which every connection you opened yourself satisfies: a node nobody could reach looked healthy and stayed leech-only. A background probe connects to the address a tracker publishes for you and completes a BitTorrent handshake, which only your own client can answer, and the dot follows that. Unknown is shown as such rather than as success.
- **One dot per engine.** They listen on different ports and a port forward can cover one and miss the other.
- **Both addresses are shown on a dual-stack host**, since being reachable over one family says nothing about the other.

## v3.78.1 - 2026-08-16

### Fixed
- **A browser could keep serving an old copy of the WebUI.** The page itself was already sent with no-cache, but the scripts and stylesheets carried no cache header, so a browser could hold onto them on its own terms and show bugs that were fixed versions ago on that machine only. They are revalidated now.

## v3.78.0 - 2026-08-16

### Added
- **Apply a client spoof to every tracker in one action.** `POST /api/announce/clients/bulk` and a button in the Trackers tab. Spoofing tends to be an all-or-nothing decision, and doing it host by host is where entries get missed. An empty prefix clears it everywhere the same way.

### Fixed
- **Trackers you have configured stay listed while nothing is announcing.** The listing is built from announces that actually happened, so pausing everything emptied it, exactly when someone is likely to be reconfiguring a tracker. Hosts carrying a spoof or a passkey are now always shown, with no torrent count.

## v3.77.7 - 2026-08-16

### Changed
- **Per-tracker client spoofing and announce IP modes moved to the Trackers tab.** Both are per-tracker settings, but neither was listed under any domain, so they landed in the Other catch-all.

## v3.77.6 - 2026-08-16

### Changed
- **The race drain settings moved to the Session Race tab.** They govern that engine's disk, so filing them under Maintenance split one engine's settings across two tabs.

## v3.77.5 - 2026-08-16

### Fixed
- **The default shown for `data_dir` was wrong.** The settings page announced `/configs`, the shipped config uses `/config`.

## v3.77.4 - 2026-08-16

### Changed
- **Connectivity settings now live only in the Network tab.** Ports, interface, IPv6 and proxy credentials also appeared in the flat Session Race and Session Hoard lists, where they could be set one at a time: a SOCKS5 host without an announce proxy is exactly the combination that relays the traffic while the tracker still records your own address. The Network tab writes them as a set, so it is now the only place that offers them.

## v3.77.3 - 2026-08-16

### Changed
- **The VPN interface is picked from a list instead of typed.** The host already knows which interfaces exist, and a typed name invites a typo or a plausible wrong pick. A value already in the config that matches nothing is kept in the list rather than silently dropped.
- **The network interface card no longer sits above every settings page.** It was informational noise everywhere except the one place the interface is chosen, which is now the picker itself.

## v3.77.2 - 2026-08-16

### Changed
- **The settings save button is now a full-sized button at the end of each panel.** It was a small control in the card header, away from the fields being edited and off screen once a panel scrolls, which is also where the result banner sits: the page now scrolls to it after a save.

## v3.77.1 - 2026-08-16

### Fixed
- **The Network tab's fields no longer form a staircase.** They were plain inputs stretching to whatever space the label left them, so each row started at a different place. They now use the same fixed-width field as the rest of the settings page.

## v3.77.0 - 2026-08-17

### Added
- **A torrent whose files have gone missing is now parked in an error state instead of silently rejecting every request forever.** Until now a torrent could keep its seeding state, keep being announced and keep accepting peers long after its data had disappeared, answering each request for a piece with a reject and nothing else. Nobody was told: not the peers, who kept asking, and not the user, whose torrent looked healthy. A read that fails with "no such file or directory" now moves the torrent to `error`, records the path that could not be opened so it can be read from the torrent's details, and stops both serving and announcing it. Only a missing file triggers this. A transient failure such as running out of file descriptors leaves the torrent alone, so a storage hiccup cannot take a whole catalogue down. Recovery is deliberate: restore the files and recheck, the same as qBittorrent's missing files state.

## v3.76.2 - 2026-08-16

### Fixed
- **The connectivity check gave no sign it was running.** It takes up to a minute, and the button stayed idle throughout, so the click looked like it had done nothing. The button now reads "Checking" and is held disabled until the report arrives.

## v3.76.1 - 2026-08-16

### Fixed
- **The inbound test now proves that the thing answering is your client.** A bare TCP connect was not enough: measured on a real VPN tunnel, the provider accepts every port from inside its own tunnel, forwarded or not, so a port no peer could ever reach was reported as open. The probe now completes a BitTorrent handshake for a torrent the engine actually holds, which nothing else can answer, and reports the peer id that replied.

## v3.76.0 - 2026-08-17

### Removed
- **The announce mode that stopped announcing to a single tracker, added in v3.72.0, has been withdrawn.** A client able to fall silent on one tracker while remaining in its swarm is what private trackers screen their whitelists for, and they screen on whether the client can do it rather than on how it is used: from the tracker's side there is nothing to observe but an absence, so a careful use is indistinguishable from a ratio cheat, and no amount of care on our part is visible to them. The honest need it was meant to serve is already met, and met better, by pausing the torrents: that emits `event=stopped`, so we leave the swarm openly instead of disappearing from it. A tracker still carrying the withdrawn setting in `[announce_ip_modes]` is announced to normally again and logs a warning at startup, rather than being dropped without a word.

## v3.75.1 - 2026-08-16

### Fixed
- **The VPN mode now says when the interface picked is not a tunnel.** Picking an ordinary interface such as eth0 sends peer connections outside the tunnel, or nowhere at all, and the check reports that as the likely cause instead of a bare dial error.
- **The inbound test no longer claims a probe came from inside the local network when it left through a tunnel.** A refusal is now only reported as closed when the probe genuinely reached us from outside, through a proxy; through a tunnel or on a direct setup it turns around at the provider or the router, so the result is inconclusive rather than negative.

## v3.75.0 - 2026-08-16

### Added
- **The connectivity check now tests inbound reachability.** It opens a TCP connection to the exact address and port a tracker hands out for you, leaving by the route peer connections use. Behind a proxy or a tunnel that connection really does arrive from outside, so the answer is firm; on a direct setup it goes out and back over the same WAN address, which tests the router's loopback rather than the outside world, and the report says so instead of giving a verdict it cannot support.

## v3.74.4 - 2026-08-16

### Fixed
- **The connectivity check called a working VPN a leak.** It compared the announce address against the daemon's own address, but inside a tunnel every path shares one address, which is correct rather than suspicious. It now compares the announce path against the peer path, which is what actually catches a relay that carries the traffic without the identity, and it states plainly that a check run from inside a tunnel cannot see an address that exists outside it.

## v3.74.3 - 2026-08-16

### Added
- **French translation of the Network tab.** All 62 new strings, including the mode descriptions, the warnings and the connectivity report.

## v3.74.2 - 2026-08-16

### Fixed
- **The SOCKS5 mode no longer presents itself as a complete setup.** A plain SOCKS5 proxy carries outgoing connections only, so the address announced through it answers nobody and only self-initiated peer connections work. The mode card, the warnings and the connectivity check all say so now, and the check reports inbound reachability as failed rather than untested.

## v3.74.1 - 2026-08-16

### Changed
- **Clearer wording on the Network tab's proxy field.** It explained yesterday's bug instead of saying what the field does, and read as though two addresses were being handed out.

## v3.74.0 - 2026-08-16

### Added
- **A Network tab in the settings, with the connectivity setups as four choices.** Direct, VPN, SOCKS5 proxy, or SOCKS5 plus a PROXY-v2 relay. Only the chosen mode's fields are shown, and saving clears the keys of the other three, so a half-finished attempt from last week cannot survive as something that looks deliberate. The proxy is entered once and wired to both peer connections and tracker announces, which is the pair that used to come apart silently.
- **A "check what actually happens" button.** It measures the address a tracker sees, through the announce path itself, next to the address peers see and this host's own address. When a relay carries the traffic but not the identity, the three no longer agree and the report says so. Setups that cannot work are refused at save time with the reason in words, and environment variables that override the page are listed instead of applying behind it.

## v3.73.0 - 2026-08-16

### Added
- **`announce_proxy`, so tracker announces can be relayed too.** `socks5_outbound_*` only ever covered peer connections; announces were issued by a different code path that read one environment variable and nothing else. A relay configured entirely in the config file therefore hid the peer traffic while the tracker still recorded the host's own address. Set `announce_proxy = "socks5h://user:pass@host:port"` under `[race]`/`[hoard]` to send them through the proxy as well. UDP trackers are skipped while it is set, because SOCKS5 carries TCP only.
- **`announce_ip`** fills the BEP-7 `ip=` announce parameter. Empty (the default) omits it and lets the tracker observe the source address, which stays the right answer for nearly every setup.

### Fixed
- **A proxied peer setup no longer leaks its address in silence.** When a session dials peers through a SOCKS5 proxy but has no announce proxy, startup now says so plainly instead of leaving the operator with a setup that looks correct from every angle they can check.

## v3.72.0 - 2026-08-16

### Added
- **A "none" mode to stop announcing to one tracker.** Set it from the Trackers tab, or as `"none"` under `[announce_ip_modes]`. The tracker is skipped silently rather than recorded as failing, so switching one off does not leave a permanent error in the list, and no rate-limit token is spent on a request that is never sent.

## v3.71.1 - 2026-08-16

### Fixed
- **The announce IP selector could not be used.** It sat in a table row that carries a torrent count and a last-announce time, so the row is rewritten on every poll and the open dropdown was torn out mid-click. The family now moves to the Edit form beside the client spoof and the passkey, and the row shows it read-only.

## v3.71.0 - 2026-08-16

### Added
- **Announce IP column in the Trackers tab.** The per-tracker address family was only reachable by editing the config or calling the API, which made it a setting nobody would find. Each tracker row now carries an auto/v4/v6 selector that applies on that tracker's next announce, and the listing reports the mode in `ip_mode`.

## v3.70.0 - 2026-08-16

### Changed
- **Dual-stack hosts now announce on both address families.** Most trackers record only the address an announce arrives from, so leaving by one family made us unreachable for peers on the other — with nothing failing and nothing logged. Hydra now announces once per family with the same peer_id, one peer at two addresses, which is what libtorrent has always done. A family the host does not have, or that a tracker has no address on, is not tried.
- **`[announce_ip_modes]` "auto" means both families**, not the kernel's pick. Pin `v4` or `v6` for a tracker that miscounts the pair or caps peers per account.

## v3.69.0 - 2026-08-16

### Added
- **Per-tracker announce address family.** On a dual-stack host a plain dial prefers IPv6, so announces leave over v6 and a tracker that records only the announce source address holds a v6 address for us — IPv4-only peers then get a peer entry they cannot dial, with no error logged anywhere. Set `v4` (or `v6`) for a tracker under `[announce_ip_modes]`, or hot via `POST /api/announce/ip-modes`; the default `auto` keeps the previous behaviour. Ignored when an announce proxy is configured, since the egress family is then the proxy's.

## v3.68.0 - 2026-08-15

### Added
- **A Re-check paths button in the import wizard.** The reachability figure was computed once, against the mapping Hydra guessed, so correcting a mapping left the warning stale until you pressed Import and found out. The button re-tests the folders you have typed and marks each row, and it only stats those folders rather than re-listing the library, so it stays instant however large the library is.

## v3.67.0 - 2026-08-15

### Fixed
- **Checkboxes were drawn as full-width empty boxes.** A global rule styled every `input` as a text field, `appearance: none` included, so a checkbox lost its tick and stretched across the dialog. The import wizard's "import everything stopped" option was the visible casualty: it has been there since v3.54.0 and on by default since v3.62.0, but nobody could recognise it as a switch, let alone see that it was already set.
- **A partial torrent could re-download data it already had.** Before adding, the engine probes the disk to decide whether to hash-check instead of downloading, but it only looked at the torrent's *first* file. A partial download very often lacks exactly that one, and the probe then reported an empty disk for a torrent holding most of its payload. Measured on a three-file torrent with two files present: 67% recovered when the missing file was last, 0% when it was first.

### Added
- **The qBittorrent import now says when it cannot find your data.** It samples the library before writing anything and refuses to start if nothing is reachable under the current path mapping, which is what a wrong volume bind looks like. The preview reports the same count, and the final report carries a `data_missing` tally. A complete torrent whose payload is missing is no longer added in seed-mode, which would have announced us as a seeder with nothing to serve. Pass `force: true` to import anyway.

## v3.66.0 - 2026-08-15

### Added
- **`block_mse`, a hot flag that turns encrypted peer connections away.** An encrypted peer cannot use the zero-copy serve path, so it costs RC4 on every byte plus a heap copy per write. The flag refuses MSE in both directions and closes the encrypted sessions already running, since those are the long-lived ones and leaving them in place would hide the effect for hours. Off by default: refusing MSE turns away real peers, which is a trade to measure rather than a default to assume.
- Engine diagnostics gained `mse_inbound_refused`, `mse_outbound_skipped` and `mse_sessions_dropped` so a flip can be confirmed rather than presumed.

## v3.65.0 - 2026-08-14

### Added
- **Hydra updates itself on Windows.** Right-click the tray icon and pick *Check for updates*: it compares against the newest published release, asks before doing anything, then downloads the archive, verifies it against the SHA-256 published beside it, replaces the two executables and starts Hydra again. Settings and data are never part of the archive, so an in-place update is the one route that cannot lose them - unzipping a release into a fresh folder is what leaves people wondering where their torrents went.
- The archive now carries a third file, `hydra-update.exe`. It is a separate program because Windows locks a running executable, so Hydra cannot overwrite itself: the updater is handed Hydra's process id and waits for it to exit before touching a single file. Hydra stops through the tray's own Quit path, the only one on Windows that flushes resume data, so an update costs no re-check.

### Changed
- Nothing is replaced unless the whole download succeeded and matched its checksum, and if any executable fails to land the previous ones are put back. A half-applied update, a new front end against an old engine, is the outcome worth the most effort to avoid. If the updater cannot start at all, Hydra says so and keeps running rather than stopping for an update that never began.

## v3.64.0 - 2026-08-14

### Added
- **Force download, from the torrent list.** Right-click a hoard torrent and pick *Force download*: it holds a download slot from then on, ahead of the seed-rank quota and exempt from the activity cooldown. The engine has been able to do this for a while, but the only way to ask was a hand-written API call. A matching *Forced* filter chip sits next to *Downloading*.
- **Filter counts now follow the filtering.** Each group of chips is counted with the other groups applied but never its own, so picking a category makes every tracker report what it holds *inside* that category, while the tracker list stays whole and switchable. Previously every chip showed its total against the entire hoard no matter what was selected, and the numbers only refreshed when the list itself changed.

### Changed
- **Pins moved from a JSON file to the database.** They were kept in `hoard_pinned.json` beside the engine; they are now a `pinned` column on the torrent row, next to `paused`. The row dies with the torrent, so a pin can no longer outlive what it points at - the old file had accumulated 140 pins on torrents removed long ago. Nothing is carried over from it: a pin only claims a download slot, so a pin on a finished torrent meant nothing worth keeping.
- **A pin now ends when it stops meaning anything.** The slot manager drops any pin whose torrent is no longer incomplete, whether it finished or was removed, and pinning a torrent that has already completed is refused outright rather than accepted and quietly reaped moments later.

## v3.63.0 - 2026-08-13

### Added

- **Hydra now lives in the Windows notification area.** Hovering the icon shows
  the current download and upload rate and the torrent count; double-clicking
  opens the web UI; the right-click menu opens it or quits Hydra.

- **No more cmd.exe window on Windows.** Double-clicking `hydra.exe` starts it
  in the background, with the tray icon as its visible presence. Started from a
  terminal it attaches to that terminal and prints as before, so nothing is
  hidden from anyone who asked to see it; `--console` forces a console for the
  cases in between (a shortcut, a scheduler, debugging). Every line still goes
  to `hydra.log` and to the UI Logs tab either way.

  The two arrived together on purpose: with no console there is no Ctrl+C, and
  Windows never delivers SIGTERM, so removing the window on its own would have
  left no clean way to stop Hydra -- and the clean stop is what flushes resume
  data (v3.61.2). **"Quit Hydra" in the tray menu is that path.**

### Fixed

- **The engine subprocess no longer opens a console window of its own** on
  Windows (`CREATE_NO_WINDOW`), which it would have done as soon as the app
  itself stopped having one to lend it.

- **The Windows README no longer tells you to copy a password out of the
  console.** No password has been generated since v3.55.0 -- you create the
  admin account in the browser on first run. Same stale instruction as the one
  fixed for `install.sh` and the wiki in v3.61.5, missed on Windows.

## v3.62.0 - 2026-08-13

### Changed

- **Imports now land everything stopped by default.** The option existed since
  v3.54.0 but was off, so a migration started announcing before you had a chance
  to check the paths. Uncheck the box to get the old behaviour. Callers of
  `/api/import/{qbit,transmission}/start` that omit `start_stopped` are affected
  too: omitted now means stopped.

### Fixed

- **The import wizard's exit button no longer says "Skip" when opened from
  Settings**, where it cancels rather than skips. It also stopped silently
  dismissing the first-run import prompt when used that way.

- **The progress dialog stops saying "Importing..." once the import is done**,
  which contradicted the "Import complete" line right under it.

## v3.61.5 - 2026-08-13

### Fixed

- **`reset-password` now accepts `--config <path>`**, the form everyone tries
  first because it is how the daemon takes it. It used to store the literal
  string as the filename and fail with `open --config: no such file or
  directory`. The positional path still works.

- **A subcommand placed after other arguments says so instead of starting the
  daemon.** `hydra --config … reset-password …` used to boot the server and sit
  there, which reads as a hang rather than as a mistake.

- **The bare-metal instructions were wrong.** `install.sh` and the wiki told you
  to find the admin password with `journalctl -u hydra | grep -i password`, but
  no password has been generated since v3.55.0 replaced it with a first-run
  screen. On a remote host that screen is refused — it only answers a private
  network, so an instance exposed to the internet cannot be claimed by a
  stranger — which left a seedbox user with no documented way in. The installer
  and the wiki now point at `reset-password`, and the wiki has a section for
  installing without root.

## v3.61.4 - 2026-08-13

### Fixed

- **The release tarballs would not run on an older distribution.** The engine
  was built against the runner's glibc and against OpenSSL as a shared library,
  so it demanded `libssl.so.3` and glibc 2.34 — while the hosts that most want a
  bare-metal install, seedboxes, commonly run Debian 11 (glibc 2.31, OpenSSL
  1.1). The failure was immediate and opaque:

      hydra-engine: error while loading shared libraries: libssl.so.3:
      cannot open shared object file: No such file or directory

  The engine is now built against musl and links nothing at all, matching the
  Go binary which was already static. Verified running on Debian 11, Debian 10
  (glibc 2.28) and Alpine, which has no glibc. Container users were never
  affected and nothing changes for them.

  Release builds now fail if the engine links any shared library, so this
  cannot come back unnoticed.

  The tarball is a little larger as a result. If you hit this, download the
  v3.61.4 tarball and replace both binaries; your config and data are
  untouched.

## v3.61.3 - 2026-08-13

### Fixed

- **The qBittorrent and Transmission imports lost the completion date.** The
  original add date was carried over, but every imported torrent recorded as
  having finished at the moment of the import, so seeding-time rules and the
  "completed on" column read wrong for the whole library.

## v3.61.2 - 2026-08-13

### Fixed

- **The engines never saved their resume data on shutdown.** Every stop, on
  every install since the Typhon engine landed, ended with both engines killed
  after ignoring the shutdown signal for ten seconds apiece — twenty seconds of
  waiting, and nothing written. What survived was whatever the five-minute
  periodic save had last put on disk, so a restart re-hashed pieces that were
  already complete and a torrent that had just finished could come back at 0%.

  The engine now handles SIGTERM and SIGINT and flushes before it exits. A
  clean stop is also much faster: twenty seconds of dead waiting becomes well
  under one.

### Added

- **`HYDRA_STOP_TIMEOUT`** sets how long each engine gets to flush before it is
  killed (default `10s`; accepts `45s`, `2m`, or a bare number of seconds). The
  default suits an ordinary instance — a full resume sweep runs at roughly
  36 000 torrents per second on an SSD. Raise it if you hold several hundred
  thousand torrents, and raise your supervisor's stop timeout with it: the two
  engines are stopped one after the other, so the container needs more than
  twice one engine's share.

- **A shutdown budget in the shipped deployment files.** `docker-compose.yml`
  now sets `stop_grace_period: 30s` and the systemd unit `TimeoutStopSec=30`.
  Docker's own default is to kill a container ten seconds after SIGTERM, which
  is not enough for both engines. If you run Hydra from your own compose file
  or `docker run`, add the same budget — see the README and the wiki.

## v3.61.1 - 2026-08-12

### Fixed

- **v3.61.0 shipped without container images.** The new multi-architecture
  build pushed both images correctly and then failed handing their digests to
  the step that publishes the tags, so no image was published at all — neither
  arm64 nor amd64. Nothing was wrong with the images themselves, and v3.61.0's
  binaries and tarballs were unaffected.

  If you pull the container, use v3.61.1: it is v3.61.0 plus this fix. If you
  installed from a tarball, v3.61.0 is already what this release contains.

## v3.61.0 - 2026-08-12

### Added

- **`max_dials_per_sec`, a limit on new outbound peer connections** (per engine,
  under `[race]` and `[hoard]`, in dials per second, `0` = unlimited and the
  default). This is the setting that VPN users behind Proton and similar
  providers actually need. `announce_rate_limit`, added in v3.59.0, capped how
  often Hydra announced but not what each announce produced: one announce asks
  a tracker for 200 peers and every peer that comes back was dialed at once, so
  even 20 announces a second could open thousands of new flows through a single
  tunnel and knock it over. qBittorrent's equivalent knob is connections per
  second rather than announces per second, and this is ours. As a starting
  point, a few hundred dials a second is generous for most links; if a tunnel
  still drops, halve it. The engine's `get_config` now reports a
  `dial_governor` block (live connections, refusals, delays) so a limit set too
  tight is visible rather than guessed at.

- **`start_paused`, holding an engine's outbound traffic at startup** (per
  engine, default `false`). With it set, no announces and no peer dials leave
  Hydra until you press **Start now** in the banner at the top of the page, or
  call `POST /api/startup-pause/release`. It exists because the boot-time wave
  of a large library is exactly what knocks a tunnel over, and until now there
  was no way to reach the settings before it left.

  This hold is process-level and writes nothing: torrents you paused yourself
  stay paused, and releasing the hold does not resume them.

- **Official arm64 container images.** `ghcr.io/kheopsian/hydra` is now a
  multi-architecture image covering `linux/amd64` and `linux/arm64`, so a Pi,
  an Ampere box or an Apple-silicon Docker host pulls the right one with no
  change to the pull command or the compose file. Linux arm64 *binaries* have
  shipped for a while; only the container was amd64-only.

  Both architectures build in parallel on native runners rather than one of
  them through emulation, and the tags are published as a manifest list once
  both succeed — a tag never carries one architecture and not the other.

### Fixed

- **`max_connections` was doing nothing at all.** The key was parsed and
  reported back by the engine, but no code ever enforced it, so any value you
  set was silently ignored. It is now a real ceiling: at the limit Hydra stops
  dialing new peers. Inbound peers still count towards the total, and so do
  shut dialing down, but they are never turned away — refusing someone who
  wants to leech from you would trade upload against a number. The live count
  can therefore sit above the ceiling; what it will not do is get there by
  Hydra's own dialing.

  **Check your value before upgrading.** Because the setting never had an
  effect, existing values were never chosen against real behaviour, and a
  number that looks reasonable may sit well below what your instance actually
  runs. It is now enforced as written. When the key is absent Hydra no longer
  substitutes a default of its own: unset means unlimited, which is what every
  install has effectively been running until now.

## v3.60.2 - 2026-08-12

### Fixed

- **The Changelog tab showed raw `**` markers and spilled text out of its
  bullets.** The renderer worked one line at a time, so a bullet written across
  several lines ended at the first newline: the rest became a loose paragraph
  outside the list, and emphasis opened at the end of one line never found its
  closing marker on the next, leaving the asterisks on screen. Lines are now
  folded into their block before any formatting is applied, which is what a
  soft-wrapped paragraph or list item actually is. A blank line between two
  bullets no longer splits them into two separate lists either, and backslash
  escapes such as `\*` render as the character rather than the escape.

### Changed

- **The changelog is no longer confined to a narrow column**, going from 820px
  to 1200px so long-form entries use the space the page already has.

## v3.60.1 - 2026-08-12

### Fixed

- **`session_uploaded` lost bytes on every torrent you removed, and never got
  them back.** The session delta is `current total - offset captured at boot`,
  and the total is a sum over the torrents an engine currently holds — so
  removing one makes it drop. The per-engine offsets are decremented to match,
  which keeps their delta honest; a third, combined offset was not, so it drifted
  further from reality with every removal. On a library that churns, the damage
  is not subtle: measured at **2.35 TiB reported against 11.82 TiB actually
  sent**, a factor of five, inside a single afternoon. `/api/status` was
  contradicting itself as a result — `day_uploaded`, built from the per-engine
  deltas, showed five times what `session_uploaded` did for the same window.
  `getSessionDelta()` now sums the two per-engine deltas rather than keeping a
  counter of its own, and the combined offset is gone. The qBittorrent shim
  serves this number too, so anything reading session stats through it (\*arr,
  cross-seed, autobrr) was seeing the same shortfall.

- **Per-torrent `total_uploaded` / `total_downloaded` were always zero in the
  store.** The periodic sync writes every other column of a torrent's row and
  simply never wrote these two, so they sat at 0 for every torrent, forever —
  the intended writer (`UpdateStats`) had no callers at all. They are now filled
  from the engines on the same five-minute cycle, which already rewrites each
  row, so this costs no extra writes. They are written with `MAX()` rather than
  assignment: an engine still loading reports 0 for a torrent it has not reached
  yet, and an absolute write would erase that torrent's history on the next tick.

### Known issues

- **Session stats still start out counting bytes sent before this boot.** The
  offset that makes a session delta mean "since this process started" is
  captured while the engines' stats cache is still empty, so it lands at zero;
  seconds later the loaded torrents' lifetime totals show up and are booked as
  traffic from the current session. Gating the capture on the store import does
  not fix it — the capture already happens after the import. It needs to wait
  for the stats cache to reflect the loaded set. Telemetry only: announces read
  the engine's per-torrent counters, so tracker credit is unaffected.

## v3.60.0 - 2026-08-12

### Fixed

- **A torrent added in seed mode showed 0% for up to a minute.** Adding data you
  already hold is meant to be instant, but progress only caught up on the next
  60-second refresh, which reads as a stall. The `torrent_added` handler now
  reports `progress = 1.0` straight away for a seed-mode add, and the 1 Hz stats
  snapshot derives the same conclusion from the engine's own state instead of
  waiting for the slow path.

## v3.59.1 - 2026-08-11

### Fixed

- **The interface language could only be picked once, on the first-run screen.**
  After the admin account was created there was no way back to it: the choice
  lived in the browser and nothing in the UI exposed it, so anyone who clicked
  past that screen, or who joined an install someone else had set up, was stuck
  with whatever the browser had asked for. Configuration now carries a
  **Language** card next to Display units, listing the same languages. Picking
  one reloads the page, which is the honest way to do it: a DOM that has already
  been translated cannot be translated again, the English keys it was built from
  are gone by then.

## v3.59.0 - 2026-08-11

### Added

- **`announce_rate_limit`: cap how fast announces leave, in announces per
  second.** A large library announces in waves — the scheduler wakes every
  torrent whose deadline has passed and sends them all at once. Behind a VPN
  that wave is a burst of new outbound flows through a single tunnel, and some
  providers (Proton among them) throttle or drop its tail, which shows up as
  tracker failures for no visible reason. The new per-session setting puts a
  token bucket in front of every announce, so the same announces go out spread
  over time instead of in a burst. It covers both `http://` and `udp://`
  trackers. `0` (the default) keeps the previous unlimited behaviour, and
  fractional rates are allowed (`0.5` = one announce every two seconds). Size it
  above your library divided by the announce interval: 16k torrents at a
  30-minute interval need roughly 9 per second to keep up, so a limit of 20
  smooths the burst with room to spare. When the configured rate cannot sustain
  the volume, Hydra says so in the log once a minute rather than silently
  falling behind.

### Fixed

- **`enable_ipv6 = false` did not stop the announce going out over IPv6.** The
  setting governed the peer listener, the tracker's `peers6` list and the
  self-dial filter, but not the announce, which is the only part a tracker
  sees. Go's dialer prefers IPv6 wherever the host has it, so a tracker
  registered a v6 address for someone who had asked for none. The announce is
  now pinned to IPv4 when the setting is off. A configured SOCKS proxy is left
  alone, since the egress family is then the proxy's; a host with no IPv4 says
  so once at boot rather than failing every announce separately.
- **`enable_ipv6` never appeared in an upgraded config.** It was missing from
  the settings written into existing `default.toml` files, so an install made
  before 3.58.0 had no line to find or change: the setting was off, and
  invisible.

## v3.58.4 - 2026-08-11

### Fixed

- **Adopting an orphaned category proposed the wrong engine.** The form opened
  on race with an empty save path, so adopting a label whose torrents all live
  in hoard moved every one of them to the other engine, with nothing on screen
  to say so. The orphan listing now reports the engine most of a label's
  torrents are actually in and the directory most of them sit under, and the
  form starts from those. It proposes rather than imposes, since a majority can
  be unrepresentative; an empty save path never wins that vote, so the real
  directory of a minority beats a blank one.

## v3.58.3 - 2026-08-11

### Added

- **A database that moved to a share can be converted from the interface.** A
  database created on local disk is journalled with WAL, which a share cannot
  host: the fallback opens with `nolock`, and SQLite refuses `nolock` on a WAL
  file outright. Pointing `data_dir` at a share therefore left an install that
  would not start, for a reason nothing on screen explained. Hydra now checks
  before it opens, and when that is what happened it comes up in repair mode:
  no store, no sidecar migration and no engines, since a daemon that runs on
  without its store rewrites its carry-over files from an empty memory. The
  interface explains it and offers one button, which copies each database to a
  `.bak`, converts it, verifies it opens the way a share will, and asks for a
  restart.

### Fixed

- **A JSON sidecar that came back could overwrite the store.** The one-shot
  import of the carry-over files was unconditional, and it overwrites rather
  than merges. Every sidecar is renamed aside once imported, so finding one
  again means a boot that could not open the store wrote it from an in-memory
  state that had never been loaded. Upgrading out of that state then imported
  those files over the real numbers: a lifetime upload counter became a file of
  zeroes, and a category list became whatever single screen its owner had
  opened while degraded. A sidecar is now imported only when the store has
  nothing of its own to lose; anything else is set aside unread as
  `.superseded` and reported at boot.
- **A category label with no category can be adopted, not just deleted.** When
  a definition goes missing the torrents keep the name, and the only action
  offered on the leftover was Delete, which strips the label off every one of
  them. Adopting it defines a category with that name, and since a torrent
  points at its category by name, every torrent already wearing it joins.
- **The categories table stopped flickering.** It was painted twice per
  refresh, once before the orphan list arrived and once after, and rebuilt in
  full every second even when nothing had changed.

## v3.58.2 - 2026-08-11

### Fixed

- **An install that already had a database stopped opening it.** The filesystem
  probe that decides whether `data_dir` needs the network fallback was asked
  about `data_dir/hydra.db` rather than about `data_dir` itself. Where that file
  already existed the probe resolved to the file, then tried to create its test
  socket inside it, which cannot work; it read the resulting `ENOTDIR` as a
  share refusing a socket and put a perfectly local install onto the network
  fallback. That fallback's connection string carries `nolock=1`, and `nolock`
  cannot open a database in WAL mode, so the store failed to open on every boot
  with `SQLITE_CANTOPEN` and Hydra carried on without it: no categories, no
  lifetime counters, and the JSON sidecars rewritten from an empty in-memory
  state. A fresh install has no database file yet, so the walk-up landed on the
  directory and everything worked, which is why only upgrades were affected.
  The probe now asks the directory that would actually hold the socket.

## v3.58.1 — 2026-08-11

### Fixed

- **The engine could dial itself over IPv6.** Turning IPv6 on in production
  opened 8348 connections whose local address was also the remote one: the
  engine calling itself on a Docker bridge address. The self-dial filter only
  ever held our detected *public* addresses, which is all IPv4 ever needed,
  since a machine's own addresses are RFC1918 there and no peer list can hand
  one back. Under IPv6 every address on the host is globally routable, the
  wildcard listener accepts on all of them, and a box running Docker carries one
  per bridge. The filter now also holds the host's own IPv6 addresses, loopback
  and link-local aside, refreshed on the same two-minute push. Installs without
  IPv6 push exactly what they pushed before.

## v3.58.0 — 2026-08-11

### Added

- **IPv6, off by default, per engine.** `enable_ipv6` under `[race]` and
  `[hoard]` makes an engine listen for peers over IPv6 and accept the IPv6
  peers trackers and PEX hand out. Left off, nothing changes: the engine binds
  v4 only, exactly as before, and a tracker that volunteers `peers6` still gets
  its v6 list dropped. Turned on, a v6-only listener is added *beside* the v4
  one rather than replacing it, so v4 peers keep their v4 addresses everywhere
  in the UI, the dedup and the stats. Enable it only where IPv6 actually works:
  announcing an address nobody can reach costs connections.
- **Our own IPv6 is detected, not configured.** With the setting on, the public
  v6 address is looked up the same way the v4 one already was, and pushed to the
  engine self-dial filter alongside it. Without it a tracker handing our own v6
  back to us would have us dialling ourselves. Off, no v6 lookup is made at all.

### Fixed

- **IPv6 peers handed to the engine were silently dropped.** `add_peers` built
  its socket address as `"{ip}:{port}"`, which is only ever valid for v4: a bare
  v6 literal came out as `2001:db8::1:6881`, failed to parse, and the peer was
  skipped without a word. The address is now parsed on its own and paired with
  the port. This also unblocks the v6 peers the DHT already returns.

## v3.57.0 — 2026-08-11

### Added

- **The interface speaks other languages.** A language selector sits on the
  first-run screen, and the choice survives a reload. French is complete across
  the whole UI; German, Spanish, Italian, Dutch and Portuguese cover onboarding
  and navigation. Translating is editing one JSON file, no Go and no JavaScript
  required. The key *is* the English text, so English can never regress: a
  missing entry renders the key, which is already the right English, and a
  partial translation is a valid state rather than a broken screen.

### Fixed

- **The upload chart follows your speed unit again.** On the Benchmark tab it
  had its formatter hard-coded to the *size* unit, so it printed KiB/s whatever
  you had chosen, while every other rate in the UI honoured the setting. It now
  reads the speed unit like the rest, in bytes/s or bits/s.
- **Three French strings no longer leak into the English UI.** The bulk-add
  result line and three chart labels had been written in French and shipped
  that way to everyone. They are English again, and translated properly.
- **The per-tracker chart label reads correctly.** It said `Chart tracker`,
  which parses as neither the chart nor the tracker. It is `Tracker chart`.

## v3.56.0 — 2026-08-10

### Added

- **UDP trackers work.** Announces to `udp://` trackers (BEP 15) were skipped
  outright, so a torrent whose trackers are all UDP had only the DHT to find a
  swarm on. Both announcers now speak it: connect, announce, and a peer list in
  either address family. Connection ids are cached per tracker rather than
  renegotiated for every torrent, which at 100k torrents over a handful of
  trackers is the difference between a normal client and a flood.
- **A UDP announce refuses to run outside your proxy.** SOCKS5 carries TCP; a
  datagram cannot be relayed through it without UDP ASSOCIATE, which Hydra does
  not implement. Rather than quietly send the announce direct, and with it your
  real address, an announce is skipped while `TYPHON_ANNOUNCE_PROXY` is set.

### Fixed

- **The engine package's tests compile again.** They had not since v3.53.0: the
  magnet work added a method to the client interface without updating a test
  stub, so the whole package was skipped, including the download-slot tests that
  guard against churn regressions seen in production.

## v3.55.0 — 2026-08-10

### Added

- **You pick your own admin password on first run.** A fresh install no longer
  invents one: it opens a setup screen and asks. Hydra used to generate a
  password early in boot and only report it at the very end, so a start that
  failed in between left the account taken and the password nowhere, with no
  way in short of `hydra reset-password`. Nothing is generated now, so nothing
  can be lost. Setup answers only while no account exists, and only from
  localhost or a private network. `admin-credentials.txt` is gone with it: a
  plaintext password next to the config, written too late to help.
- **Config on network storage is detected and supported.** Pointing `data_dir`
  at a CIFS/SMB, NFS, 9p, AFS or CephFS mount used to fail at boot with
  `database is locked`, because SQLite's write-ahead log needs a shared memory
  file no share can provide, and because the engines' sockets cannot exist on
  one at all. The store now falls back to a rollback journal held under an
  exclusive lock, the sockets move to local scratch, and a warning says so on
  startup and in the web UI. Read the warning: without the write-ahead log, a
  share that drops mid-write can corrupt the database, so keep backups or keep
  `data_dir` on a local disk. Your downloads can live on the share either way.

### Fixed

- **Torrent data on a share was never the problem.** It only ever needed
  positional reads and writes, which every network filesystem does; it was
  `data_dir` that could not move. The two are now separate concerns.

## v3.54.1 — 2026-08-10

### Fixed

- **A complete torrent that has never run now reports 100%, not 0%.** Seed-mode
  torrents carry no piece map, so one imported stopped read as empty until it
  was started. It is also, at last, no longer re-announced by the race seed
  keepalive while stopped.

## v3.54.0 — 2026-08-10

### Added

- **Import everything stopped.** Both import wizards (qBittorrent and
  Transmission) now offer a checkbox that lands the whole library stopped, so
  you can try Hydra on a real library without a single announce going out.

## v3.53.1 — 2026-08-10

### Fixed

- **A magnet placed on a remote agent now resolves through the right tunnel.**
  The resolution was handed to whichever engine came first on the agent, so a
  hoard placement had its metadata fetched by the agent's race engine. Each
  engine dials out through its own binding, so the lookup — and the address
  peers saw — left by the wrong route.

## v3.53.0 — 2026-08-10

### Added

- **Magnet links work, on race, hoard and remote agents.** A magnet carries no
  metadata, so there was nothing to place: race refused them outright and hoard
  silently dropped the URI and added nothing. The info dict is now fetched from
  the swarm first (BEP 9 ut_metadata) and turned into a real .torrent, which
  then takes the existing add path — so placement, save_path rules and qBit
  shim behaviour are unchanged. The add returns immediately rather than
  blocking on the swarm, and the torrent shows as resolving (metaDL to qBit
  clients) until its metadata lands. Resolution runs on whichever node will
  hold the data, so a remote agent resolves over its own network, and always
  leaves through a configured binding rather than the default route.

- **Hydra serves metadata as well as asking for it.** Peers resolving a magnet
  can now fetch the info dict from us. The raw dicts are not held in memory;
  the .torrent is re-read from disk on the rare occasion a peer asks.

### Note

- Magnet trackers are HTTP(S) only for now: Typhon has no UDP tracker client,
  so udp:// entries in a magnet are skipped and the DHT covers those swarms.

## v3.52.2 — 2026-08-10

### Fixed

- **The restored categories now survive the first save.** The v3.52.1 repair
  wrote them back and lost them minutes later: the boot import dropped the
  store's record for every torrent the engine had already rebuilt from resume
  data, which carries no category, and the next sync wrote that blank view over
  the repair. The import now hands those fields to the engine, no sync runs
  before it finishes, and the repair is allowed one more pass.

## v3.52.1 — 2026-08-10

### Fixed

- **Categories lost on upgrade are restored.** v3.50.0 retired `state.json`
  without moving the categories into the store, so every torrent added before
  that release came up uncategorised. They are read back from the
  `state.json.migrated` the upgrade left behind, once, on the next start.

## v3.52.0 — 2026-08-09

### Fixed

- **Outgoing encrypted connections work again.** The MSE handshake read the
  peer verification constant without skipping PadB, so it landed in the padding
  and every handshake to an encryption-only peer failed. Measured on 15 live
  swarms: 0 of 45 outgoing MSE handshakes succeeded before, 10 of 78 after.

### Added

- **Dial diagnostics.** The plaintext and MSE legs now count separately
  (`dial_plain_*`, `dial_mse_*`), the dial queue is accounted for, and
  `TYPHON_DIAL_TRACE_IH` traces a single info_hash through every dial decision.

## v3.51.0 — 2026-08-09

### Fixed

- **The listen port shown is the one the engine is bound to.** The port-forward
  panel, the Engines table and the qBit shim all read the boot-time config, so
  a hot rebind left them showing a dead port — and the health check probing it.
- **A failed rebind no longer answers OK.** The native endpoint, the shim and
  the gluetun hook all reported success regardless, which left a node on a port
  nothing forwarded. A port set at runtime is now written back to the TOML, so
  it survives a restart.

### Added

- **`POST /api/v2/app/setPreferences`** in the qBit shim, the route VPN
  port-forward scripts expect. `listen_port` is applied; other keys are
  accepted and ignored, as qBittorrent does.

## v3.50.0 — 2026-08-08

### Changed

- **One home for durable state.** The category list, the import provenance and
  the content-layout flag move into the SQLite store, next to the torrents they
  describe. The TOML keeps the daemon's configuration. Existing files are
  imported on first boot and renamed aside, never deleted.
- **`state.json` is gone.** It was the last thing rewritten in full — tens of
  megabytes on every save — for data the store already held.

## v3.49.1 — 2026-08-08

### Fixed

- **The import panel in Config no longer says qBittorrent only.** It opened the
  wizard that offers both clients, but the heading hid Transmission.

## v3.49.0 — 2026-08-08

### Added

- **Import from Transmission.** Point Hydra at Transmission's config folder — or
  upload a zip of it — and it takes the lot: save paths, labels as tags, upload
  history, add dates, and which torrents were stopped. It reads the folder
  rather than the RPC, so Transmission does not even have to be running, which
  is usually how a migration goes. Optionally creates one category per
  destination folder.

### Fixed

- **Imports keep the state torrents had in the client they came from.** A
  torrent you had paused in qBittorrent or Transmission arrives stopped instead
  of quietly starting to seed.

## v3.48.0 — 2026-08-07

### Added

- **Stop and start, the way qBittorrent 5 means them.** A torrent you stopped
  now reads `stopped` and stays that way; one held back by a scheduler reads
  `queued`. Both have their own filter chip, and the qBit shim answers to
  `/torrents/stop` and `/torrents/start` (`pause`/`resume` still work) with
  `stoppedUP`/`stoppedDL`.
- **Stopping tells the trackers you are leaving.** Hydra now sends an
  `event=stopped` announce instead of just going quiet, so the tracker stops
  counting you as an active peer straight away.
- **Bulk stop/start on a whole filter.** Ctrl+A selects everything the current
  filters match, not just the rows on screen, and the action travels as the
  filter rather than as one hash per torrent.

## v3.47.0 — 2026-08-07

### Added

- **PUID/PGID support in the container.** Set both and Hydra drops privileges at
  start, so what it writes belongs to your user instead of root — what the *arr
  stack needs to hardlink it. Unset (the default) still runs as root. Switching
  an existing install over? Chown your data directory first: Hydra only chowns
  its config, never your payload.

### Fixed

- **An add into a directory Hydra cannot write is refused, with the reason.** It
  used to be accepted and then sit in `downloading` forever, failing silently
  whenever a piece arrived.

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
