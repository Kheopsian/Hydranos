# Changelog

All notable changes to Hydranos are documented here. This project follows
[semantic versioning](https://semver.org).

This file is compiled into the binary and served at `/api/changelog`, so a
release with no entry here is a release that cannot describe itself. CI checks
that the top entry matches `HYDRA_VERSION` (`.github/scripts/version_guard.py`).

Two ways to title a new entry:

- `## v<major>.<minor>.<patch> -- title`, matching `HYDRA_VERSION`, when you
  know the number this will ship as.
- `## Unreleased -- title`, when you do not. Preferred for a branch that will
  sit for a while: this repository has merged as many as four minor bumps in a
  day, so a number chosen while writing a branch is often taken by the time
  anyone reviews it. Whoever tags the release renames the heading and sets
  `HYDRA_VERSION` in the same commit.

### The reconcile stops amplifying a failed load

`spawn_store_reconcile` deletes store rows whose torrent the engine does not
hold. That reads "could not be loaded" as "delete it" -- and the row is where
the metainfo blob lives, so five minutes after a collision kept a torrent from
loading, its last copy was gone and no fix could bring it back. Reproduced on
the bench: torrent A, perfectly legitimate, was erased between two restarts.

Two limits, both on a pure `rows_to_drop` so they can be tested by breaking
them:

- a record the loader REFUSED keeps its row -- that row is what the next start
  repairs from;
- a pass that would drop more than 1% of a session (floor 50) refuses and says
  so. A partial load looks exactly like a mass deletion from here, and this
  worker cannot tell them apart. The existing guard only caught an engine that
  loaded NOTHING; this catches the one that loaded almost everything.

## Unreleased -- the facts cache comes back out

### Data we already hold is recognised, whatever it is called

A torrent's `pieces` field hashes the payload stream, and that stream carries
no names: not the files', not their folder's, not the torrent's. `info_hash`
covers all three. So two torrents that differ only in naming have identical
`pieces` -- which turns "do we already have this?" into an exact lookup rather
than a guess.

Hydranos now keeps that lookup. A `content_index` table maps each torrent to a
key derived from its piece hashes, filled at boot for anything not yet indexed
and maintained on every add. With `[dedup] mode = "auto"`, an incoming torrent
whose payload we already hold is hardlinked onto the existing files under its
own names and seeds immediately, instead of downloading bytes that are already
on the disk.

Measured on a 301 221-torrent catalogue before building this: 69 493 groups of
identical payload covering 163 274 torrents, 9 336 of those groups stored in
more than one place, for 25.76 TiB of duplicated data. 22.20 TiB of it sits
within a single filesystem, where a hardlink reclaims it outright.

Three things the implementation is careful about, each of which costs real data
when skipped:

- The index narrows, the bytes decide. A key match is confirmed by comparing
  the `pieces` blobs before anything is linked, so no collision can attach one
  torrent's name to another's data.
- Identical `pieces` guarantees the same byte stream, not the same cut through
  it. The file size list is compared before pairing files by index.
- Directories are created, never linked -- the kernel refuses `link()` on a
  directory -- and a partial failure removes everything it just created. A
  half-linked torrent would seed corrupt data to the swarm.

Cross-filesystem matches are reported and skipped: `link()` returns `EXDEV`
there, and copying would defeat the purpose.

`[dedup] enabled` is one switch, off by default: linking destroys nothing, but
it writes to the filesystem, and a feature that starts doing that on upgrade
unasked is a bad surprise.

There is deliberately no "just tell me about it" middle setting. The duplicates
come from automatic imports -- an *arr, autobrr, a batch ingest -- and none of
those has anyone in front of a screen, so a queue of offers nobody reads is
dead weight that still has to be maintained.

`scripts/dedup_sweep.py` applies the same reasoning to what is already stored.
It is a dry run unless given `--apply`, and every removal is preceded by a link
whose inode is verified against the source.

Added in 4.23.0 on the reasoning that rebuilding the session facts cost 0.78s
of every request. It does -- but the cache was measured never to serve: 0 hits
in 15 requests against a daemon that had been up an hour, latency flat at
0.45s, while the store took one write every 20 seconds. Neither staleness nor
write pressure explains that, and a cache whose behaviour nobody can explain is
a liability rather than an optimisation. A cache on this same path had already
been removed once for growing with the catalogue.

Nothing measurable is lost: the 2x on the page came from the lighter row
decoding, and the 9.5x on the qBit shim came from asking the index for the one
category *arr wanted. Neither depended on this. 21 MB and a TTL go with it.

What remains is worth doing properly: of the 0.45s, the query is 0.20s and the
rest is decoding it -- 300k Strings and a HashMap grown from empty.

## v4.27.0 -- who the other end actually is

The Client column knew six clients. Everything else -- BiglyBT, Tixati,
PicoTorrent, WebTorrent, KTorrent, and the pre-Azureus conventions entirely --
showed its raw two-letter code, or nothing at all when the peer id followed no
convention the table recognised. The one real peer this production node had
connected at the time read as `lt 0D80`, a client the table did not know
printing a version nobody decodes.

`peer::peerclient` replaces it: ~90 Azureus codes, the Shadow style that
predates them (`T03C---` is BitTornado 0.3.12), Mainline (`M4-20-8--`), and the
handful that put their name up front (`exbc`, `XBT`, Opera).

Version strings are rendered the way each family writes them, which is not one
rule: qBittorrent counts in decimal (`5220` is 5.2.2), libtorrent in base 16
(`0D80` is 0.13.8), Transmission uses a two-digit minor (`3000` is 3.00), and
ours base 36 so a minor above nine fits one character. Unparseable characters
fall back to the raw string rather than inventing a number.

An unknown code still reports the code, and 20 random bytes report nothing --
that is a real answer, not a failure: a client is free to send them, and many
do.

The table was previously duplicated byte for byte in `peer::choking` and
`tracker`, one copy dead. There is one now.

## v4.26.0 -- the peer id tells the truth, and tells the same story twice

### It said 2.4.3.0

`-HY2430-` has been the fingerprint since the first public commit, on a daemon
now at 4.x. The four characters of an Azureus-style peer id ARE the version --
every client that decodes them, ours included, read a number that was never
true, and published a build number besides.

It is derived from `HYDRA_VERSION` now. One character per component means base
36, not decimal: qBittorrent writes 5.2.2 as `-qB5220-`, which works while
every component stays under ten. Hydranos is at minor 26, so decimal would need
five characters and stop being a peer id. `-HY4Q00-` for 4.26.0.

### The tracker was told one thing and its swarm another

`announce_clients` spoofs the client per tracker -- but only in the ANNOUNCE.
The handshake kept the engine's own id, so a private tracker was told
`-qB5220-` while every peer in its swarm saw `-HY....-`. A tracker that
compares the two, and strict ones do, sees a client lying about what it is:
worse than not spoofing at all.

The torrent now carries the prefix its FIRST tracker calls for, and all three
handshake paths use it -- outbound dial, inbound plaintext, inbound MSE. Set by
the announce runner on every cycle, so an override added at runtime applies
without a restart.

Only the eight-byte prefix is replaced; the random tail stays the binding's.
That is what keeps two engines of one node distinguishable, and the
self-connection guard -- which compares peer ids -- working.

A torrent with no override keeps the binding's id. That is every public
torrent, and it is the right answer there: DHT and PEX hand over peers with
nobody vouching for them and nobody cross-checking.

## v4.25.0 -- the detail panel stops dying on the first connected peer

A torrent with connected peers froze its detail panel. The peer flags arrive as
a STRING, the way qBittorrent writes them -- "UFS" is three flags -- and both
panels read them as an array. `.map` threw on the first peer, and since the
tracker table is drawn just below in the same function, it was never drawn at
all: the panel kept whatever it showed when it opened.

Which reads as the tracker never having been announced to. It had: the API
reported status ok, last announce 163s, next in 1637s, scrape 2s/2l, while the
panel said otherwise. The bug was never in the announce.

Both panels are affected, and race always has peers -- its detail panel has
been frozen since the V4 for any torrent actually transferring.

`Array.from` takes either shape, so an older daemon sending an array still
renders.

Underneath it, a second one the crash was hiding: the peer table read
`down_speed` / `up_speed`, which the V4 engine does not publish -- it sends
`dl_rate` / `ul_rate`. Both names are read now, so the columns fill whichever
answers.

## v4.24.0 -- the store owns the metainfo, and the resume stops lying

### Torrents that could not be deleted and came back at every start

A hoard library carried 540 torrents the engine served but neither database
knew: absent from `hydra.db` and from `<engine>/state.db`, invisible to the
detail panel (404 while the list showed the row), refused by DELETE (every
write route resolves through the store first), and back after a restart --
seeding and announcing a payload that in some cases had already been erased.

The cause is that the metainfo is stored TWICE. The store keeps it as a blob
keyed by info-hash; `uploads/` keeps the same bytes in a file. The resume
record pointed at the FILE, by path -- and until the V4 that file was named
after whatever the client had called its upload. A batch ingester posting every
torrent as `t.torrent` overwrote one file 2789 times. Measured on the
production library: 995 paths shared by 5648 records, and 4728 records whose
key no longer matches the torrent their file parses to.

At startup the record's key was never compared to the file: `load_resume_data`
inserted under the hash the FILE parsed to. A record keyed A pointing at a file
holding B restored B, under a hash nothing had a row for.

Two changes, in that order:

- **The store is now the source of the metainfo.** The blob is fetched by the
  record's own info-hash, so the key IS the identity and there is nothing left
  to disagree with. A record the store has never heard of -- an install
  predating the store -- still falls back to its file.
- **A record that disagrees with its file is refused**, counted, and reported
  at the end of startup instead of restoring the wrong torrent. Restoring it
  was worse than restoring nothing: the result could not be deleted, shown, or
  stopped.

The 883 torrents whose record diverged but whose blob the store holds are
repaired by the first change rather than lost to the second.

The uploads directory (725 481 files, 16 GB against 4.12 GiB of blobs for the
same 300k torrents) is still written, so a rollback to 4.23 still starts. It is
no longer what a restart trusts.

## v4.23.0 -- stop rebuilding the library on every request

Measured on the production library, a filtered hoard page took 0.82 s and
`/api/v2/torrents/info?category=series` took 1.5 s. Walking the engine's
300 853 torrents accounts for 0.04 s of that. The rest was the store: the facts
of the WHOLE session rebuilt from SQLite on every single request, so `limit=1`
cost the same as `limit=500` and narrowing a filter bought nothing.

### The qBit shim asks for the category it wants

*arr polls by category and nothing else. The shim was building a StoreFacts for
each of 300k torrents -- three Strings and a Vec apiece -- to keep the 1972 in
`series`. `Store::facts_in_category` asks the covering index for that category,
and the shim walks those hashes instead of the catalogue.

The shortcut is exact for a named category: a torrent with no row in the store
has no category either, so it can only fall under the engine's own name. That
one case still takes the full walk, which is also what keeps torrents the
engine holds but the store has forgotten visible to `category=hoard`.

### The page's facts are cached until the store is written to

Keyed on `sqlite3_total_changes()`, SQLite's own tally of rows written on the
connection. Any INSERT, UPDATE or DELETE moves it, so the cache cannot go stale
through a write nobody remembered to annotate -- the failure mode that makes a
hand-maintained version number untrustworthy across sixty write methods. It
does not see writes made on another connection, so a 60 s TTL bounds that.

Only the SLIM facts are cached: 40 bytes a torrent against ~200, so ~21 MB at
300k and ~86 MB at a million, against a 2.5 GiB RSS. A cache over the rich
facts existed on the listing path once and was removed for growing with the
catalogue; this is deliberately not that. It also replaces an allocation of the
same size that was being made and dropped several times a second, so under
concurrent requests it lowers the peak rather than raising it.

## v4.22.0 -- tracker errors you can act on, and chips that count again

### Tracker errors, grouped by what they ask you to do

The hoard list gains a fourth facet family: the KIND of tracker error. On the
production library, 1931 torrents in error carried only 13 distinct messages,
and those 13 collapse to four classes -- dead on the tracker, bad passkey,
rate-limited, tracker unreachable. Grouping by raw message would have split the
one class worth acting on across five spellings in two languages, which is the
split the operator is trying to undo.

The chips filter like the other three families: left click includes, right
click excludes, and the counts come from the server over the whole library. The
row is hidden entirely while nothing is in error.

A message carries both stacks (`v4: ... | v6: ...`) and the halves disagree
often enough to matter. Each half is classified and the most actionable wins,
so a torrent unregistered over v4 and merely unreachable over v6 reads as
unregistered.

New query parameters on `/api/<engine>/page`: `error_class=` and
`error_class_not=`, comma-separated, same shape as `category` and `tracker`.
The facet block gains `error_class`.

### Facet counts survive a second node

Enrolling one node blanked every number on the hoard page: the chip counts, the
three facet families, and the "All" total. The fan-out answered `facets: null`
on the reasoning that nodes need not share a category or tag vocabulary.

They need not -- but the UI reads the same block for the state chip counts, and
refusing to merge turned a question with a perfectly good answer into silence.
Differing vocabularies are not a merge problem: the union of two keyed counts is
the answer, and a category only one node knows appears with that node's count.
`facets` is still null when NO node counted any, which is a different claim from
zero and the one the UI needs to decide whether to draw chips at all.

## v4.21.0 -- stacked cards get their gap back

Reverts 4.20.4, 4.20.5 and 4.20.6, and does what was actually asked.

"pas d'espace entre les sections" was a REPORT, not an instruction: there was no
space between the cards, and there should have been. Read as an imperative, it
produced three releases removing space that already measured zero, ending with
the cards merged into one slab. They are cards again.

Stacked cards now sit 16px apart -- the same gap the `.cards` grid already uses
elsewhere, applied to cards that stack in normal flow. Adjacent siblings only,
so a card following a `.cards` grid does not get a second helping.

Kept from that run, because they were real and measured: the title band padded
once (4.20.3, 65px against Account's 40px on the same page) and the
Personalisation card's own internal layout (4.20.2).

## v4.20.6 -- no dead band at the join either

4.20.5 removed the double rule and the notches; a ~29px empty strip was still
sitting at every join. Measured: 16px of card-body bottom padding, the 1px rule,
then 12px of the next card's title padding, with nothing drawn in any of it.

A body pads 16px all round, which is right when the card ends there and wrong
when another starts immediately below. Rows already breathe 7px of their own,
so the bottom pad was the part doing nothing. Dropped, for cards that have a
sibling after them.

## v4.20.5 -- stacked cards are one panel

The answer, at the fourth asking, was the seam between the cards themselves.
It measured **0px** every time I looked, which is exactly why three rounds of
hunting for a margin found nothing: there is no gap. Each card draws its own
1px border, so where two touch the join is a **2px double rule**, and each
rounds its corners at 12px, so the page background shows through **four
notches** at the join. That is the space.

A run of adjacent cards now reads as one panel: the second and later drop their
top border and top radius, and any card followed by another drops its bottom
radius. Adjacent siblings only, so a card sitting after a `.cards` grid is left
alone.

Applies wherever cards stack -- Overview, Race, Hoard, Workflows and Config all
had it.

## v4.20.4 -- no space between the sections

Third attempt at the same sentence, and the first one aimed at the right thing.
The blocks the maintainer meant are the ones the code literally calls sections:
`.settings-section`, the groups with the blue monospace titles inside Advanced
settings -- Language, Display units, Network interfaces, then one per TOML
table.

Measured: 18px between Language and Display units. `.settings-section` carries
`margin-top: 12px`, and the first three also carried an inline
`margin-bottom: 18px`; the two collapse to 18.

Both are gone. Sections stack flush -- the title already separates itself from
its rows by 4px and the last row above already pads 7px, so the seam reads
without a band of empty space.

## v4.20.3 -- the title band, padded once

Measured rather than eyeballed: on the Config page the title band of
Personalisation and of Advanced settings is **65px tall**, Account's is **40px**.
Same page, same markup shape, 25px apart.

`.card h3` carries `padding: 12px 16px`, because in a plain card the heading is
the band. A card with a header button wraps that heading in `.card-header-row`,
which carries the same padding -- and the two stacked. `.card-header-row h3`
reset the margin and not the padding, so it survived every look at this page.

Those 25px are what read as space between a section and its content. The
heading no longer pads inside a row that already does.

## v4.20.2 -- the Personalisation card is one block

the maintainer, on the screenshot: "pas d'espace entre les sections". The card read as
three loose pieces because both headings were `.settings-row` -- a row built for
a label facing a control on the right, so it pads top and bottom and rules a
line underneath. Here the control is the grid directly below, and that padding
was opening a gap between a heading and the very thing it names.

Headings are plain blocks now, with the space put ABOVE them and never below:
a heading floating midway between its own control and the previous one belongs
to neither.

## v4.20.1 -- 403, because that is what qBittorrent says

With 4.19.0 deployed and the right credentials saved, Sonarr and Radarr still
refused their own connection test: **"Unable to connect to qBittorrent"** --
while `curl` from inside the Sonarr container logged in and read the version
without trouble. The network was fine and the credentials were right.

qBittorrent's WebUI answers **403 Forbidden** to a call with no session. The
shim answered 401. Sonarr's `QBittorrentProxySelector` probes
`/api/v2/app/webapiVersion` *before* logging in and reads a 403 as "log in
first"; anything else it reads as "this is not a qBittorrent", and reports it
as a connection failure. So the one code that meant "authenticate, then retry"
was the one code the shim did not send.

A small middleware turns a 401 into `403 Forbidden.` on `/api/v2/*` only. The
native API keeps 401, which is the correct code and what its own callers
expect; this is compatibility with one client's reading of another server's
quirk, and it is scoped to the paths that imitate it.

## v4.20.0 -- docker stop actually stops

`docker stop -t 300 hydra-go` was in every deployment script in this repository
and it had never once been graceful. The daemon is PID 1 in its container, and
PID 1 does not get the default disposition of SIGTERM: with no handler
installed, the kernel **discards** the signal. So the stop sent a SIGTERM that
nothing received, waited out the full timeout while the daemon carried on
accepting peers, and then SIGKILLed a 300k-torrent instance. The script printed
"stopped" and moved on.

It was visible the whole time and nothing looked wrong: `docker stop` exits 0
whether it drained or shot the process. What gave it away was the log -- new
inbound peers, four minutes after the stop began.

3.x saved on the way out (`src/main.rs` still does). The port to a single Rust
process dropped that path along with the binary that held it, so from 4.0.0
every deploy threw away up to five minutes of resume state -- piece progress and
byte counters, written by the five-minute sweep -- and the next start re-checked
what it lost.

- **SIGTERM and SIGINT are handled**, through axum's `with_graceful_shutdown`:
  the listener drains, then every engine writes its resume state.
- **Bounded, and honest about it.** `HYDRA_STOP_TIMEOUT` (default 120s) is a
  budget, not a guarantee; the alternative to a partial sweep is the SIGKILL
  that follows, so a sweep cut short is still strictly better than none. It
  logs how long it took and warns when it overran.
- **One thread per engine**, so a slow disk on one does not spend another's
  share of the budget.
- A handler that fails to install leaves its branch pending rather than
  resolving, so a failed SIGINT registration cannot fake a shutdown or mask
  SIGTERM.

## v4.19.1 -- the light theme, actually light

Driving the new panel in a real browser rather than trusting the CSS: Daylight
painted the page white and left the header and the tab bar dark, with a white
"HYDRANOS" on a white background. The colours were right; five literals in the
header were never tokens.

- `--logo-text`, `--logo-glow`, `--logo-subtitle`, `--nav-bg` and the two
  hairline stops are theme variables now. Abyss keeps its exact values, so the
  default is unchanged to the byte.
- Daylight sets `--logo-glow: none`. A text glow on a light background is a
  smudge, not a highlight.
- The Tabs list showed a checkbox labelled "Trackers7": `textContent` on the
  tab swept up its count badge. It reads the text nodes only.

## v4.19.0 -- Hydranos, an honest tracker row, and a page you can shape

Three things, none of which change what the daemon does to your torrents.

**The name.** Hydra is taken: `apt install hydra` gets you THC-Hydra in six of
the seven package repositories that carry the name, which makes "install Hydra"
an instruction that does the wrong thing. Every user-facing string, the page
title, the header, the README and the docs now say **Hydranos**.

Deliberately NOT renamed, because each one would break a running install for
no gain: the `HYDRA_*` environment variables, the `hydra` binary and its
config paths, the `hydra_*` browser storage keys (renaming those signs
everyone out), the SQLite files, and the GitHub repository. The peer id
prefix never carried the name at all -- it mimics qBittorrent on purpose --
so nothing a tracker sees changes.

**A tracker row says which of three things happened.** The detail panel
reported "Success" for a tracker it had never spoken to. `last_error` being
empty was read as "it went well", when on a fresh boot it means "nothing has
happened yet" -- and on a 300k catalogue that is every torrent, for about an
hour, because the scheduler admits 500 per ten seconds on purpose.

- The API now sends `status`: `never`, `ok` or `error`. Empty is no longer
  evidence of success.
- A torrent still queued shows **not announced yet**, in a muted style: not an
  error, not a success.
- Its next announce is no longer a dash. The scheduler publishes how much of
  the catalogue it has yet to admit, and the row shows the drain time as an
  upper bound -- `~1h40m max` -- captioned as an estimate for the queue, not a
  time for that torrent.
- The row was rendered by two identical copies, one per panel. It is one
  function now; three states and an estimate is not something to keep in step
  by hand.

**Personalisation, in Config.** A theme picker -- Abyss (unchanged, the
default), Slate, Ember, Kelp, Daylight -- and a list of the top tabs with a
checkbox each, so an instance that never races or never uses workflows can
stop carrying the tab. Overview and Config always stay, because Config is the
way back to the panel.

Both live in this browser's storage, not in `default.toml`: it is one person's
view of one browser, and an operator hiding a tab must not hide it for
everyone on the instance. Hiding a tab hides the link and nothing else -- no
endpoint closes, nothing stops running. It is not a permission.

The theme is applied by a four-line inline script in `<head>`, before the
stylesheet paints, because choosing it from `app.js` at the end of the body is
a dark flash on every light theme.

**The qBittorrent login also takes the API key.** 4.18.0 gave the *arr stack a
way in through the admin account, which is correct and still works -- but it
means an operator whose clients are already broken has to know a password
nobody wrote down, when every one of those clients is holding the API key
right now in a field the daemon had stopped reading. The password box accepts
either. It is not a weaker credential: it is the same secret the header
carries, over the same connection, and a caller holding it can already drive
the whole API. Compared in constant time, never echoed back, and an instance
with neither a key nor an admin account still authorises nobody.

**Also:** the header polygon's tooltip said "3 agents". The dots are engines,
one vertex each, and every other word on that panel already said so.

## v4.18.0 -- the *arr stack can log in

Sonarr, Radarr, autobrr and cross-seed configure a "qBittorrent" with a host, a
port, a username and a password. None of them can set `X-Api-Key` and none of
them can add a query parameter, so the API key -- the only credential this
daemon accepted -- was one they had no field for.

They were never *asked* for one either. Until 4.15 an instance that kept the
published placeholder key authorised every caller outright, so the clients
worked by walking through an open door. Closing that door in 4.15 left them
with no door at all, and the failure was silent in the worst way: they POST to
`/api/v2/auth/login`, which answered `Ok.` to any credentials and set no
cookie, and then took a 401 on every request after it. A client that has just
been told its login succeeded reports "cannot connect" and nothing says why.

- **`/api/v2/auth/login` is a real login.** It verifies the username and the
  bcrypt password hash of the admin account -- the same account the WebUI signs
  in with at `/api/login` -- and answers `Ok.` or `Fails.` with a 200 either
  way, because that is what qBittorrent answers and the clients parse the body,
  not the status.
- **A session is a cookie, and `authorised` accepts it.** `SID`, 32 bytes of
  system entropy, `HttpOnly` and `SameSite=Lax`, valid for an hour of
  inactivity. Deliberately not `Secure`: the *arr stack speaks plain HTTP over
  the LAN, and a cookie it can never send back is not a session.
- **Sessions live in memory and nowhere else.** A restart logs every client
  out and every client logs back in, which is also what qBittorrent does, and
  it stops a stolen cookie outliving the process.
- **A session is not a way past the key rules.** An instance with no key
  configured still authorises nobody, cookie or not; that is asserted, because
  a second branch in `authorised` is a second place for the 4.14 hole to come
  back.
- **`/api/v2/auth/logout` ends the session** instead of returning 200 and
  leaving it live.

## v4.17.2 -- the reannounce button announces

Pressing Reannounce did nothing, and said it had worked. `reannounce_one`
looked the torrent up, found it, and returned `{"status": "ok"}` without asking
anyone to announce anything -- there was no way to request an announce at all,
so the button had never worked since the Rust port. Every report of "the
tracker is not updating" had no lever to pull.

- **The scheduler takes bumps.** It owns its heap of deadlines and never shares
  it, so a bump is a message on a channel like everything else: the hash is
  pushed with a deadline of now and comes out of the heap first.
- **A bump carries an epoch, and a stale deadline is dropped.** The torrent
  already had a deadline sitting in the heap; pushing a second one without a way
  to tell them apart meant the old one fired later and announced a second time.
  A `BinaryHeap` cannot remove from the middle, so the superseded deadline is
  discarded when it surfaces.
- **A torrent the scheduler has not admitted yet can still be bumped.** The
  catalogue joins 500 per ten seconds, so a fresh torrent in a 300k install can
  be over an hour from its turn, and "wait an hour" is not an answer to someone
  pressing the button.
- **One bump per torrent per minute.** The button jumps the queue; it must not
  become a hammer. A private tracker notices an account announcing the same hash
  ten times a minute, and that is the only harm this could do. Same floor the
  scheduler already applies between two announces.
- **The channel is per engine, never a global.** A `OnceLock` shared by the
  process is exactly how the egress setting leaked between two engines, and a
  bump delivered to the wrong scheduler announces the wrong catalogue.

The route now answers 404 for a hash nobody holds, 503 for an engine that is
loaded but not on the network, and 429 when the scheduler is too busy to read
its channel -- rather than "ok" for all four.

## v4.17.1 -- workflows

The automation people otherwise write as a cron script, in the app: conditions
in, actions out, checked on a timer. File a torrent under another category once
it finishes; seed for two days then stop.

Conditions form a tree of AND/OR groups rather than a flat list. A flat list
forces the same rule to be written three times the first moment somebody wants
"tracker A or tracker B", which is the shape qui settled on and the reason.

- **29 fields**, each with the operators its kind allows -- a duration is never
  offered "contains", a tag never ">=". The catalogue is one constant the
  compiler validates against and the editor builds its dropdowns from, so a
  field cannot exist in one and be missing from the other.
- **Preview before enable.** A new workflow starts disabled, and preview runs
  the same evaluation the pass runs. A preview computed separately would be a
  preview of something else.
- **Convergence**: a torrent already in the state a rule asks for is matched but
  not acted on, so a workflow settles instead of rewriting the same tag every
  fifteen minutes.
- **A cap per pass**, reported when it bites, so a mistyped rule cannot touch a
  whole catalogue in one go and the operator can see why only some moved.
- **Its own interval per workflow**, measured from its own last run rather than
  from startup, with a sixty-second floor.
- **Seven days of activity**, refusals included with their reason.
- `delete` may not share a workflow with another action.

`seeding_time` is deliberately absent from the field list. The column exists in
the store and 4.x never writes it -- the 3.x seedtime counter was not ported --
so it reads zero for every torrent. Offering it would hand out a condition that
looks right and silently never fires. "Seed for two days then stop" is
`completed_age >= 2d`, which is a real measurement.

Conditions compile once into closures. A pass looks at every torrent, and
parsing "500GiB" per torrent per rule is the difference between a background
task and a stall. `500GB` and `500GiB` are different numbers and neither is
reinterpreted.

A duration that has not happened -- the completion age of a torrent that never
finished -- is NaN, not a sentinel. Every finite stand-in satisfies "completed
less than a day ago", which would match the entire unfinished catalogue.

## v4.16.1 -- pause stops the transfer, not just the row

Pausing a torrent wrote a column and nothing else. The transfer carried on:
`9d0e5efc` was measured going from 54% to 70% in forty seconds, at 8 MB/s,
while the interface showed `stoppedDL` throughout. It finished its 2.1 GB
download in that state.

Nothing disagreed anywhere, which is why this survived. The displayed state is
derived from the stored intent (`row::derive_state`), so it reported the click
rather than the disk, and every screen agreed with every other one.

Three independent defects, each enough on its own:

- **The decision never reached the engine.** Every pause path -- the native
  routes, the bulk routes, pause-all, and the qBittorrent shim -- wrote
  `torrents.paused` and stopped there. `stop_torrent()` existed and worked;
  nothing outside the scheduler ever called it.
- **The scheduler would have undone it anyway.** The download slot manager
  started any incomplete torrent inside its ceiling without asking whose
  decision had stopped it, so it lifted manual pauses within one interval. The
  3.x design had a gate for this that was not carried over.
- **The engine's own stop did not stop a transfer.** `stop_torrent` sets a
  flag, sets the status and drops the torrent off the DHT -- but nothing on the
  peer path ever read that flag. Sessions already connected went on requesting
  blocks and went on serving them, so a pause halted new peer discovery and
  nothing else. Only the webseed path checked it.

Now: a pause reaches the engine; the engine stops asking for blocks and stops
serving them, which is what makes the bytes stop; the slot manager treats a
paused torrent as invisible rather than as a candidate, so it frees its slot
instead of holding one; and the intent is restored into the engines at startup,
which a restart used to silently discard.

Blocks already in flight when the pause lands still arrive -- the request
pipeline is bounded, so that is a few dozen kilobytes and then silence. Peers
stay connected and choked rather than being dropped.

A resumed torrent re-enters the queue like any other and can show `queued`
before `downloading` when the ceiling is full. That is the queue working, not
the resume failing.

## v4.15.0 -- an instance with no key authorises nobody

⚠️ **Security. Read the upgrade note: a client that sent no API key and worked
anyway will now get 401.**

A fresh Docker install served its entire API, and its entire configuration, to
anyone who could reach the port. Proven on a 4.14.0 container with a virgin
`/config`: `/api/status`, `/api/settings`, `/api/engines` and `/api/categories`
all answered 200 with no credentials at all, while a *wrong* key was correctly
refused with 401. Sending nothing succeeded; sending something failed.

Two ported defects met. 3.x generated a random API key on first boot and
persisted it; the Rust port dropped that step while still shipping a template
carrying `api_key = ""`. And `authorised()` ended in `provided == expected`,
where a missing header becomes `""` -- so the absence of a key matched the
absence of a key. `/api/settings` returns the whole TOML, which is to say
`password_hash`, `agent_token`, `api_key` and the Sonarr/Radarr keys, in one
unauthenticated request.

- **`authorised()` fails closed.** An empty configured key now refuses every
  caller rather than admitting the one who sends nothing.
- **The daemon generates its own key at first boot**, 24 bytes of system
  entropy written back into the config file -- the 3.x behaviour, restored.
- **The published placeholder is no longer a bypass.** `change-me-in-production`
  used to switch authentication off entirely once an admin password existed,
  which is the state production ran in, so production authenticated nothing. It
  is now compared like any other key, and replaced at boot on any install still
  carrying it.
- **Keys compare in constant time.** A bearer secret checked with `==` leaks its
  prefix to anyone willing to time enough requests.
- **`POST /api/setup` is wired.** It answered 501 and pointed at a
  `hydra reset-password` command that does not exist, so no fresh install could
  create its admin account: the only way in was writing a bcrypt hash into the
  TOML by hand. It now creates the account and returns the generated key, which
  is the one moment the browser can learn it. It still refuses a non-local
  caller, and still 409s once an account exists.

### Upgrading

Both the `/api/v2/*` qBittorrent shim and the native API sit behind this one
check, so this is the release where a client with no credentials stops working.
Give Sonarr, Radarr, autobrr and cross-seed the API key from your config file.
If yours still said `change-me-in-production`, it was replaced on this boot --
the startup log says so, and the new value is in the file.

## v4.14.0 -- a torrent lives in an engine, and may live in several

`torrents` was keyed on `info_hash` alone, so one torrent was one row naming one
engine. That is the wrong shape. A node running one engine per tunnel has a real
reason to seed the same content from several at once: three tunnels are three
egress paths, and when the tunnel is what saturates, three copies are three
times the upload. They cost nothing extra on disk, because they are the same
files.

The table is now keyed on `(info_hash, session)`. SQLite cannot alter a primary
key, so this rebuilds it inside a transaction: either the new table is complete
or the old one is untouched, and a failure cannot leave a half-copied catalogue.
Measured on a real database: 27 rows, sub-second, catalogue intact and re-keyed.
Production's is 4.5 GB, so budget minutes and that much free space.

⚠ This ends the 3.x rollback. 3.x reads the same file and assumes one row per
hash; two rows would show it the same torrent twice. The V4 lineage is the
rollback path from here.

**Actions name the copy they act on.** The selection has always carried
`(hash, engine)`; the API took only the hash, and `find_torrent` answered with
whichever engine it met first. With one torrent in three engines that is three
different requests wearing one URL. Every per-copy route now reads `?agent=` --
the label the row already carries -- and a `<node>-...` label is deliberately
left unmatched, so acting on a remote row cannot silently hit this node's own
copy instead.

Deletion follows the same rule, with one addition: the FILES go only with the
last copy. Two engines seeding one payload share it, so removing it with the
first would leave the others seeding nothing.

`POST /api/torrents/:ih/copy` seeds an existing torrent from a second engine.
No transfer and nothing extra on disk -- both engines are pointed at the same
files -- so what it buys is a second identity in the swarm: its own peer_id, its
own port, its own tunnel. That is what pays when the tunnel saturates before the
leechers do, which is the case a multi-tunnel node exists for.

`POST /api/engines/:id/pause` exists because `hoard` and `race` had literal
routes and nothing else did: pausing the copy held by `vpn1` had no endpoint to
call, and the interface fell back to the hoard route, which paused a different
copy.

**The selection universe spans the fleet.** Ctrl+A asked only this node; the
other nodes' copies were never in it, so selecting everything and acting on it
quietly skipped them. Remote answers are merged with their labels rewritten from
`local-<engine>` to `<node>-<engine>`, the same rewrite the row merge does and
for the same reason. Measured on the bench: 29 copies, 22 here and 7 on the
other node.

**Two membership checks compared an ID where they meant a ROLE.** The detail
panel answered "torrent not in race" for every copy held by an engine that plays
race without being called `race` -- which is every extra tunnel. Same mistake
`connect` made this morning when it handed those engines hoard's network, in a
different file. Both now compare the role, and each copy reports its own state:
one `stopped`, the other `seeding`, from the same torrent.

**Ctrl+A keeps the engine.** The selection universe answered a flat list of
hashes and the interface rebuilt it with `agent: "local"` hardcoded, so
selecting everything and pausing it acted on whichever copy the lookup met
first. It now returns `(hash, agent)` pairs across every engine of the role, and
keeps the flat list for a node with one hoard engine, where the two say the same
thing.

**Peer injection and engine moves name their copy too.** A peer injected into
the wrong engine dials from the wrong tunnel -- the exact failure this model
exists to avoid -- and may dial from one that is paused. A move without a named
source moves whichever copy was found first.

**The counter worry was unfounded, and worth saying so.** `total_uploaded` on
the torrents table is neither read nor written; it is a column carried for 3.x.
What the interface shows comes from the engine, which now means per copy, which
is correct without any change. Only a per-torrent aggregate across copies would
need summing, and nothing asks for one.

**The lists show every engine of a role.** A page bound to the engine ID showed
`hoard` and hid `vpn1`, whose copies existed, seeded and were invisible.
Role-mates are merged exactly like remote nodes rather than folded into
`engine_page_value`: that function is the tightest path in the build, and reusing
the merge keeps one set of sorting and paging rules instead of two that drift.
Measured on the bench: the race page reports `local-race` 3 and `local-vpn7` 3,
and a duplicated torrent shows twice -- running in one engine, stopped in the
other.

The store methods split along a rule worth stating, because it decides what
happens to every field: **what identifies the torrent is shared, what describes
its execution is per copy.** Category, tags and the .torrent blob are the same
whichever engine holds it. Pause and pin are not -- a torrent held back on one
tunnel keeps seeding on the other. `set_session` now takes the SOURCE engine,
since "set its session" has no single meaning once there are several copies and
updating them all would silently collapse three into one. `delete_copy` drops
one and leaves the rest.

The qBit shim keeps a torrent-wide form: Sonarr and Radarr have no notion of
engines, and "stop this torrent" means all of them.

Six tests cover the rebuild, its idempotence, two copies coexisting, pause not
leaking between them, deleting one, and moving one. The pause test earned its
keep immediately: the signature had gained a `session` while the SQL still said
`WHERE info_hash = ?1`, which compiles -- the parameter is merely unused -- and
pauses every copy. A rename that builds and lies is the failure this class of
change is made of.


## v4.13.0 -- a node is a whole Hydra, and an engine keeps its own network

**An extra engine bound the wrong socket.** `local_engines` merges each
engine's session from its role profile and its own overrides, and `connect`
threw that away: `match id { "race" => config.race, _ => config.hoard }`. So
anything that was neither race nor hoard -- one engine per VPN tunnel, the
entire point of "one agent, one engine" -- listened on hoard's port and hoard's
interface. Two engines would have collided in silence, and the network tab
would still have shown the configured port, because `netprobe` reads the
`Engine` fields, which were right all along. `Engine` now carries its merged
session and `connect` uses it. The announce mode and the race drain follow the
ROLE too, not the id: an engine named `vpn1` with `role = "race"` is a racer.

**The fleet is a list of Hydras, not of agents.** `/api/nodes` holds other
whole Hydra instances, each addressed by URL and its own API key, and reached
over the ordinary HTTP API this build already serves. There is no private
protocol: a remote capability is the same route as the local one and cannot rot
apart from it -- which is precisely what happened to the agent surface, where
eleven handlers ended up answering a plausible error and doing nothing.

The registry lives in the STORE, not in `default.toml`. Declaring a remote node
used to mean hand-editing a TOML over SSH, which is the single reason nobody
used it. Nodes are probed concurrently, so six dead ones cost one timeout, and
an unreachable node reports WHY -- a refused key and a refused connection need
opposite fixes and looked identical before.

`DELETE /api/nodes/:name` answers 404 when there was nothing to remove.
Its predecessor `delete_agent` returned `{"status":"ok"}` unconditionally
while doing nothing, so the row disappeared from the table and came back on the
next reload with no error to explain it.

**`/node/:name/open` opens a node's own front, already authenticated.** A
redirect to the remote ORIGIN carrying its key in the URL fragment, not a
path-prefixed proxy: the front asks for absolute paths, which under a prefix
this Hydra would answer itself. A fragment reaches no server, no access log and
no Referer; app.js moves it into that origin's localStorage and strips it,
leaving the key exactly where typing it by hand would have put it.

**A node's engines can send data BOTH ways.** Pulling is the mirror of a
handoff, and the easier direction: to pull we already know where the far side is,
because its address is the node URL, where pushing had to hand the target
`auto:<port>` and let it work out ours. `POST /api/nodes/:name/fetch` takes the
metainfo over HTTP, adds the torrent into a named local engine, and dials the
remote engine on the port its own `/api/engines` reports -- read, not assumed,
since an engine on its own tunnel listens where its own session says.

A remote row can also be moved between two engines of the node that already
holds it. Relayed rather than reimplemented: it is that node's own local move,
which costs it nothing either. Doing it here would mean pulling the payload and
pushing it back to the machine it never left.

Verified as a round trip: a torrent deleted here with its files, then pulled
back from the other node into `vpn7` -- `[download] complete!`, 28672 bytes on
disk, dialled at `192.168.99.200:16472`.

**Local duplication is refused for a different reason than first stated.** The
old code called it a filesystem hazard -- two engines on one set of files, two
writers the first time either repairs a piece. That is only true while a torrent
is incomplete or being repaired; seeding the same complete files from two
engines is a legitimate thing to want, and it is what two separate clients on
one machine already do.

The real obstacle is the store: `torrents.info_hash` is the PRIMARY KEY, so
there is one row per torrent and it names one `session`. The schema cannot say a
torrent lives in two engines. Allowing it means keying on `(info_hash, session)`
-- a migration of the table 3.x also reads, and every per-hash lookup with it.
The menu now says so instead of claiming a hazard that is not the blocker.

**A torrent can move between two engines of THIS node, and that is the cheap
case.** The picker only listed remote engines, so the most ordinary move -- hoard
to a VPN-bound engine, to change which tunnel a torrent seeds from -- could not
be made from the interface at all.

`POST /api/torrents/:ih/engine` re-homes it without transferring anything: both
engines read the same filesystem, so the payload stays exactly where it is and
only ownership changes. The target is added in seed mode, since rechecking a
payload that did not move would cost hours of disk for nothing. The store's
`session` column is updated directly -- `insert_torrent` is an INSERT OR IGNORE
and would have left the row pointing at the old engine, which is how a torrent
ends up running in one engine and listed under another.

Duplicating locally is refused, shown and explained rather than hidden: two
engines pointed at one set of files are two writers on the same bytes the first
time either repairs a piece. "Why can I copy to another machine but not to the
engine next door" is exactly the question the menu should answer.

Destinations now match where the selection actually lives. A remote row is
offered its own fleet's other engines only: moving it to a local engine would
act on this node's own copy, which may be a different torrent or none at all.
And nothing is ever offered the engine it already sits on.

**The torrent context menu sends to an ENGINE, grouped by node.** A torrent
always lives in an engine; the node only says where that engine runs. Offering
"a node" was the wrong shape -- it left the far side to guess which engine, and
a category can only name a MODE, never the third engine of a multi-tunnel host.
The group is called Node, and inside it every entry reads `node2-vpn1`.

Both verbs are back. Duplicate hands the torrent over and keeps the local copy.
Move does the same, then waits for the far side to report complete before
dropping it here -- a move cannot delete now, because the target receives the
metainfo long before the bytes. The wait is bounded at six hours and fails safe:
a timeout, a network fault or a restart of this process leaves both copies,
which is a duplicate to clean up rather than data gone.

It replaces a menu that was dead twice over: it read `/api/agents`, whose
entries carry no `name`, so its list filtered itself empty and it always
answered "no other agent to send to"; and it posted to `/api/jobs/move-remote`,
which refuses every request. The group's own visibility was decided from that
same empty list, so whether it appeared had nothing to do with the fleet.

**`POST /api/torrents/upload` works, and takes an engine.** It was a `refuse!`
stub -- the route the farm's `sw_fill` uses. `placement` collapsed a category to
`race` or `hoard`, so no add path could reach any other engine whatever the
config said; it now honours an explicit engine, and an engine that does not
exist is refused rather than quietly landing the torrent in race.

**The Location column names the engine, not just the node.** A remote row reads
`node2-hoard`, because a remote torrent is in a remote ENGINE. The remote page
already labels its own rows `local-<engine>`; the merge swaps the prefix.

Removed with it: the five-second poller that fetched `/api/agents/torrents` --
an `empty_list_route!` answering `[]` -- and then dropped every row whose agent
was not local and not in that answer. That is exactly the merged remote rows. It
never ran only because its guard tested the same always-empty list; pointing
that guard at the real node list would have deleted the fleet from the table
every five seconds.

**The interface says engine where it means engine.** "Agent" was made to carry
both an engine and a machine, which is why neither could be explained. Twenty-nine
visible strings now say engine: the overview cards, the whole network panel, the
restart notices, the startup-pause line, the WireGuard wording.

The torrent tables' `Agent` column is called **Location**, because that is what
it answers. Its value is `local-<engine>` for a row held here and the NODE name
for one held elsewhere -- it spans both levels of the model, and calling it
either one would be wrong. Unifying the two spellings is a separate decision: a
node does not know its own name today.

Translations move with the keys rather than being dropped, so de, es, fr, it and
nl keep their sentences instead of falling back to English. The French wording
was rewritten to say moteur; the other five still read agent until a translator
passes.

Two groups were deliberately left alone. The old Agents-page form strings are
unreachable -- the tab is gone -- and renaming dead code is noise. The context
menu's "Move to agent" and its siblings drive `/api/agents` and the
`post_move_remote` route, which still refuses everything: renaming them would
promise a working feature. Both want deleting, not translating.

**Config no longer claims unsaved changes on arrival.** Opening Config and
leaving it without touching anything raised "Unsaved settings: 1 setting(s)
changed and not saved". The unsaved check compared `JSON.stringify` of the
server's payload against `JSON.stringify` of an object this file builds by hand:
same seventeen keys, same values, different ORDER, because serde sorts them and
a JS object literal keeps its own. `JSON.stringify` is order sensitive, so the
two strings differed on the first render. Comparison is now by content, sorting
keys first, which also keeps it right if either side gains a key later.

Measured both ways: on 4.12.1 the pending count is 1 with nothing touched and
the prompt fires on the way out; here it is 0 and no prompt appears.

**The network panel shows every engine, and saving it writes.** `extra_engines`
was a `vec![]` literal, so a node running one engine per tunnel -- the point of
the whole model -- showed race and hoard and hid the rest. The front had
rendered and collected those rows all along; nothing ever filled them.

Worse, `post_network_mode` validated the race port and answered
`{"status":"ok"}` without touching the file. The panel reported a saved network
configuration that had never been written anywhere, which is worse than
refusing: the operator walks away believing it took. It now writes the ports,
interfaces, IPv6, SOCKS5, gluetun and PROXY-v2 keys into `[race]` and `[hoard]`,
and each extra engine's port and interface into its own block.

Two guards while writing. Race and hoard sharing a listen port is refused --
this build already had two engines on one socket once, from the other
direction. And the edit is targeted text, not parse-and-reserialise: the config
carries pages of comments explaining each knob, and a round trip through the
decoder would delete every one of them. Verified: race port 16471 to 16475, an
engine's interface written into its `[engine.session]`, its other keys intact,
and all 30 comment lines still there.

**Local engines are declared under `[[engine]]`.** `[[agent]]` meant two things
at once -- a session started here and a machine reached over the network -- which
is why neither word could be explained. A node is a whole Hydra, held in the
store; an engine is a session inside one. Existing files keep working:
`[[agent]]` is still read, and read first, so a rollback finds what it left.
Nothing new is written under the old name.

Its sub-table has to match the array it belongs to. `[agent.session]` under a
`[[engine]]` header parses as a map where a sequence is expected and the daemon
refuses to boot -- found by renaming a block by hand and watching a bench die.

**A handoff no longer needs to be told where to fetch from.** `from` is now
optional: the sender hands the target `auto:<port>` and the RECEIVER fills in the
address, because it is the only side that knows which of the sender's addresses
it can actually reach. The port is that of the engine holding the torrent, not
of the first engine on the node. Verified: a handoff with no `from` resolved to
`auto:16372`, one peer queued, `[download] complete!`, 114688 bytes on disk.

**The install script covers machines without docker.** Static musl binary for
x86_64 and aarch64, plus a systemd unit. A host with neither docker nor systemd
is told so and the script stops, rather than leaving a binary on disk the
operator believes is running.

**The cost of merging, measured against a control window.** One node registered:
3.9 to 7.2 ms. No node at all, same request: 2.4 to 3.1 ms. So a node costs a
few milliseconds, nearly all of it the HTTP round trip.

The merge is O(nodes x (offset + limit)) and does NOT grow with the catalogue --
each node sorts and pages its own, and only a window crosses the wire. What does
grow is DEEP paging: page 100 of 500 asks every node for its first 50000 rows.
Shallow paging, which is what a UI does, is unaffected.

**A machine enrols itself into the fleet.** `POST /api/nodes/enrol` mints a
single-use token and hands back one line to paste on the machine that is to
become a node. The script it fetches installs Hydra, generates that node its own
API key, starts it, and calls `POST /api/nodes/register` to join.

The direction is the whole point. The controlling Hydra never opens a session
anywhere and never holds a credential for another host, so compromising its API
cannot become code execution across the fleet. Putting an SSH client and a
private key behind a torrent daemon's web form would have done exactly that --
this build's own default API key is still the placeholder, and there has been an
auth bypass in it before.

The token is the only authority that crosses, and it is single use, expires in
thirty minutes, and is spent by one conditional UPDATE so two machines racing on
the same token cannot both register. Registering cannot overwrite an existing
node either: a token holder must not be able to repoint one the operator already
trusts.

The command points back at the address the CALLER reached this Hydra on, read
from the Host header. This process cannot otherwise know which of its addresses
a third machine resolves, and a guess produces a node that installs and then
fails to register.

**A torrent can be handed to another node, and BitTorrent moves the bytes.**
`POST /api/nodes/:name/handoff` sends the metainfo over HTTP, then tells the far
side that this node holds the data. The data itself never touches the control
plane: the receiving Hydra pulls it over the protocol both ends already speak,
parallel, resumable, throttled by the same knobs as any other transfer, and
hash-checked piece by piece. 3.x relayed every byte through `read_piece` on one
side and `write_piece` on the other -- twice over the wire, with a correctness
argument to make from scratch.

Two primitives were missing and are now there. `GET /api/torrents/:ih/torrent`
serves the .torrent from the store rather than re-encoding it, so the info dict
stays byte-identical and the info hash with it. `POST /api/torrents/:ih/peers`
injects peers through `enqueue_dial`, the same door DHT already comes through,
and names the addresses it rejected instead of quietly queueing fewer than it
was given.

`from` is required and never inferred: this node cannot know which of its
addresses the target can reach -- tunnels, NAT, several interfaces with
different fates -- and a guess would produce a handoff that reports success and
transfers nothing. Verified end to end on the bench: metainfo accepted, one peer
queued, `[download] complete!`, 114688 bytes on the target's disk, seeding.

**A node URL may not be loopback.** It is used for two different jobs: this
process probes it, and the operator's browser is redirected to it. `127.0.0.1`
satisfies the first and can never satisfy the second. Reported from the bench,
where a node probed green and opened nothing.

**An engine now says whether it is actually listening.** Pin one to an
interface that is not there and it logs, in this order and within a fraction of
a millisecond: "session started, listen=0.0.0.0:16480", "on the network,
announcing", then "peer listener failed: cannot pin the peer listener to
bind_device: No such device". Two lines of INFO stating the opposite of the
ERROR that follows them, because the listener binds in a task that has not run
when they are written. `/api/engines` reported it exactly like a healthy engine.

`peer::listen` now raises a flag once a round of binds has succeeded, lowers it
if the listener dies, and `/api/engines` publishes it as `listening`. The fleet
page shows it per engine. The daemon-side line says "engine starting" rather
than "on the network", because at that point it does not know.

This matters for one engine per tunnel: a WireGuard interface that is not up
yet leaves an engine holding its catalogue, answering the API and accepting no
peer at all. Measured on the bench: three engines `listening: true` beside one
`listening: false`, `ss` agreeing with all four.

**The lists span the fleet.** `/api/hoard/page` and `/api/race/page` now ask
every declared node for the same window, and interleave the answers. Each node
sorts and pages its OWN catalogue: nothing ships a library across the network,
which is the only shape that survives 300k torrents. To answer rows 500 to 999
each node is asked for its first 1000 -- the most any single node can contribute
to that window -- and the merge takes the slice.

The merge repeats the single-node sort key exactly, tie-break on the hash
included. Without that tie-break two rows that compare equal could swap between
requests, and the same torrent would appear on two pages or on none. Tested:
three pages of ten over a 27-torrent fleet return 27 rows, all distinct.

Every remote row carries the node it came from in `agent`, which is the field
the front already reads to decide where an action goes. A node that does not
answer contributes nothing rather than failing the request: one unreachable
machine must not hide a library that is sitting right here. Verified by stopping
a node mid-flight -- the page fell to the local 23 and came back to 27.

Facets stay the local node's, and `fields=hash` -- the Ctrl+A selection
universe -- is not merged. Both need the fleet to agree on a vocabulary, or on
what selecting across machines even means, and neither is a merge detail.

**Engines can be declared from the page that shows them.** `POST /api/engines`
writes a `[[agent]]` block and asks for a restart; `DELETE /api/engines/:id`
removes one and answers 404 when there was nothing to remove. Both were stubs:
create refused everything, delete returned "unknown agent" whatever you asked
for. `race` and `hoard` are refused, since they come from the sections every
install has rather than from a block that can be dropped, and the data of a
removed engine is left where it is.

Two refusals are worth the code. A port another engine already holds is a 409
naming that engine: the collision `connect` used to create in silence must not
come back through a form. And an interface that is not up is a 400 listing the
ones that are, because binding to a missing device fails WITHOUT saying so --
measured here, `bind_interface = "wg9"` logged "session started,
listen=0.0.0.0:16379" and "on the network, announcing" while `ss` showed no
listener at all. An engine that seeds nothing while reporting itself online is
the failure this project can least afford to ship.

Also fixed: three call sites in `announce/policy.rs` still passed eight
arguments to a `prepare` that takes nine, so `cargo test` did not compile at
all on this tree. 141 tests pass.


## v4.12.1 -- the listing answers the category it was asked for

`/api/v2/torrents/info` ignored every argument it was given. The V4 port read
the query string only to authenticate, then built a row for every torrent in
every engine, so a client that asked for one category got the whole library.

Sonarr and Radarr treat that answer as their own queue. With four Hydra
download clients configured, Sonarr was tracking 1 231 919 items and Radarr
298 971 -- the same 301 224 torrents counted once per client, ebooks and all,
each one failing an import on a title that was never a series or a film.

`filter`, `category`, `tag`, `hashes`, `sort`, `reverse`, `limit` and `offset`
all work again, on the query string or as a POST form. The category and hash
filters run before a row is built rather than over the finished list: building
a row serialises a torrent, and cross-seed, autobrr and the *arr stack each
poll this endpoint several times a minute.

A torrent with no category of its own is filtered under its engine name, which
is the name the listing reports it under.

## v4.12.0 -- a seeding torrent announces left=0 again

The V4 port derived the announce `left` from `total_downloaded`, a traffic
counter, instead of from what we actually hold:

    let left = (torrent.meta.total_size as i64 - downloaded).max(0);

A torrent seeded from data already on disk -- an inject, a cross-seed, one of
our own uploads -- never downloaded a byte through Hydra, so its counter is 0
and the announce claimed `left` = the full size: a 0%-complete leecher that
happens to be uploading. Measured on prod before the fix: **112 948 of the
295 288 complete hoard torrents (38%)** announced themselves as leechers,
including 22 103 on announce.v3x.club and 10 752 on tk.tr4ker.net. Trackers
stopped counting them as seeds and seed points collapsed.

A second effect rode along: `numwant` is `if left == 0 { 0 } else { 200 }`, so
those 112k torrents asked for 200 peers on every announce instead of none.

A seeding torrent is complete by definition -- the same rule `row.rs` already
applies to `progress`. This is the mirror image of the 2.8.4 fix, which had to
stop `left` being hardcoded to 0; the port swung it the other way.

## v4.11.2 -- resizing a column stops moving the others

Dragging one column edge redistributed the rest. Two causes, both needed
fixing before the drag was local.

**Tags was the shock absorber.** `renderTableHeader` emits `data-col` only for
SORTABLE columns, and the resizer selected on it -- so Tags, the one column
with no sort, never got a pinned width and absorbed every resize on its own.
Measured on the hoard table at 2560px: widening Size by 120px took 102.7px
straight out of Tags, crushing it from 102.7px to zero. Past that point the
real columns start giving way. The resizer now keys off `data-colid`, which
every column carries, so Tags is pinned like the rest -- and gains a grip of
its own.

**`width: 100%` made the widths proportional.** Under `table-layout: fixed` a
percentage table width turns the column widths into a ratio of the container
rather than pixels, so any pixel one column gains is a pixel the others lose.
The table now carries an explicit pixel width, and a drag adds its delta to
that total: the space comes from the page, which scrolls sideways exactly as
it already did when the columns were naturally wider than the window.

Widths for every column are persisted on each drag, not just the dragged one,
so a restore has the complete set and can pin the total without measuring --
which matters because the restore runs while the tab may still be hidden.

The trade: shrinking columns can now leave the table narrower than the
viewport rather than stretching to fill it. Stretching would mean scaling the
columns back up, which is the behaviour being removed.

## v4.11.1 -- the tracker badge is a circle, and it is there on load

Two faults in the same indicator, both reported from a screenshot.

**It was never round.** The box was `min-width: 18px` against
`line-height: 17px`, so it measured 18 by 17 and the 9px radius drew an
ellipse. Height is now pinned equal to the minimum width, and the radius is
large enough to stay fully rounded when a three-digit count widens it into a
pill. The count is centred with flex instead of leading: `line-height` centres
the line box, and digits have no descender, which is what left the glyph
sitting high.

**It only appeared once you opened the tab it warns about.** `updateTabBadges`
had a single caller, inside `updateTrackers`, which runs only while the
Trackers tab is open or active. Reloading on any other tab left the badge
missing until you went and looked -- the one moment an indicator exists to
spare you. It is now called at startup and every 30s; it already made its own
`/api/announce/health` request, so it never needed the tab's data. It stays off
the 1s poll, where nothing it counts moves that fast.

## v4.11.0 -- race gets a search box

Race had none. The tab fetches the whole tier from `/api/race/torrents` and
renders every row, so finding one torrent among a few hundred meant reading the
list. `/api/race/page` -- the paginated, searchable endpoint hoard uses -- was
already registered on the server and had never been called by the front.

The box filters client-side rather than going through that endpoint: race is
served whole and already sits in the browser, so a keystroke is a filter, not a
request. It shares `_searchMatches` with hoard, so both boxes answer a query the
same way, including the tokenizing that v4.10.0 added.

`updateRaceTorrents` was split into a fetch and a `renderRaceTable`, so typing
re-renders the rows in hand instead of refetching the tier on every keystroke.

Filtering applies to the RENDER only. The stats bar describes the tier, and
computing Avg Share over the filtered set would have made it move with whatever
was typed in the box.

## v4.10.0 -- search that answers the query you typed

The filter box did one literal `contains` over the name, lowercased. Three
consequences, all measured on a demo library before touching anything:

**Two words found nothing.** Release names are punctuated, not spaced, so
`demo music` was looked up verbatim inside `demo_03_music.bin` and missed --
as would `jujutsu 1080p` against `Jujutsu.Kaisen.S02.1080p.BluRay`. Typing two
words is the reflex, and it returned an empty table on a library that held the
rows. Queries are now split on whitespace, `.`, `_` and `-`; the terms are
ANDed and their order does not matter, and the same collapsing applies to the
name, so `demo.03` and `demo 03` both find `demo_03`.

**A pasted info_hash found nothing.** The hash was only ever consulted for
torrents with an EMPTY name, and every torrent has a name, so that branch was
unreachable in practice. A query of six or more hex characters is now matched
against the hash as well, whatever the name. Six is the floor: below it,
ordinary words would start colliding with hashes.

**The name was lowercased once per row per keystroke**, on a request path
built to not allocate per field -- 300k transient Strings per request on the
production catalogue. The ASCII path now compares in place and allocates
nothing; only names carrying non-ASCII still fold, so `CAFÉ` stays findable
by `café`.

`_hoardMatches` in app.js applies the same predicate, and has to: the server
picks which rows come back, the client re-checks them so live SSE arrivals
cannot bypass the active filter. Had only the server learned to tokenize, the
extra rows would have been fetched and then hidden by the client, showing a
count with no rows under it.

## v4.6.0 -- why a tracker is unhappy, and whether it still lists us

Two instruments, both missing when they were needed.

**Failures now have a class.** `announces_failed` was one number for the whole
engine, so a node being rate limited by one tracker looked exactly like a node
announcing deleted torrents to another. Failures are counted per
`(host, class)`: `rate_limited`, `timeout`, `invalid_passkey`,
`unknown_torrent`, `dns`, `connect`, `http_error`, `other`. The class is derived
from the REDACTED message -- a raw reqwest error embeds the announce URL, and
that URL carries the passkey. On the production catalogue this would have said,
at a glance, that 107k torrents point at an archive.org that times out on every
single announce and 72k at a calewood that answers 429.

**Announces now check themselves.** One announce in 64, on a torrent that is
already seeding, asks for `numwant=50` instead of the usual zero and looks for
our own listen port in the answer. The family it appears under is the whole
point: seen on an IPv4 address, on an IPv6 one, on both, or on neither.

The verdict has three states, deliberately. A tracker usually omits the
announcing peer from its own answer, so a bare absence proves nothing. What
proves something is an ASYMMETRY -- present in one family and not the other
means the tracker kept one address and dropped the other, either because it
dedups by peer id or because one family never reached it. An absence is only
reported at all when the tracker returned fewer peers than we asked for, which
is what makes the list whole rather than truncated.

That is the failure 4.5.0 fixed, found by hand from a VPN: announces succeeded,
scrapes came back correct, and the tracker served our address to half the swarm.
Nothing inside the process could see it. Now something can.

Exposed at `GET /api/announce/health`, per engine and per host.

## v4.4.7 -- one announce client, not ninety a second

`send_announce` is new in 4.x: the Go announcer's port needed an entry point and
got one that calls `reqwest::Client::builder().build()` on every announce. That
allocates a connection pool, a resolver and a fresh load of the root certificate
store per call, and nothing it builds survives the call. reqwest documents the
client as the thing you build once and clone. `V6_PROXY_CLIENT`, twenty lines
above in the same file, already does exactly that; only the primary path was
missed.

Both versions run the same scheduler, the same 512 workers
(`hoardSchedWorkers` in Go, `WORKERS` here) and the same intervals -- the port
copied all four constants faithfully. What changed is latency per announce:
about 2.0s in 3.x against 5.7s in 4.x, inferred from the sustained rate through
a fixed pool.

The arithmetic that makes this an upload bug rather than a cosmetic one: 300674
torrents at the 2659s average interval the trackers actually ask for need 113
announces/s just to stay in their swarms. 3.x sustained 172-315/s. 4.x sustained
45-99/s -- permanently below replacement. Torrents slid past their deadline,
trackers stopped handing our address to leechers, and the node ended up present
in 2% of the swarms that had leechers at all: 3192 such torrents in a spread
sample, peers on 70. Upload followed the presence, not the serving path, which
was never at fault -- a torrent we are actually in still pushes 1.5-2 MB/s,
exactly as it did in 3.x. 74 GB/h against a 460 GB/h median for the same hour
the week before.

HTTP/1.1 is forced on the shared client. Sharing one client is what makes h2
negotiation dangerous here: every worker aimed at one tracker would land on a
single connection and serialise, trading this bottleneck for a quieter one. A
tracker announce is one small GET; h2 buys nothing.

## v4.4.6 -- pieces reserved and never given back

A second leak, uncovered by fixing the first: with the glibc allocator gone,
1.29 GB/h of real application heap became visible under what had been 11 GB/h of
allocator retention.

Two heap profiles 26 minutes apart name it exactly.
`PiecePicker::start_piece` accounts for 459.7 MB of 457.8 MB of growth -- all of
it -- reached through `peer::session::run` -> `DownloadState::get_requests`. The
webseed pool, which shares the same picker, was NEGATIVE over the window: it
releases correctly on all three of its failure paths. It was the first suspect
and it was innocent.

`start_piece` inserts a `PendingPiece` holding `vec![0u8; piece_size]`. A peer's
reservations are tracked in `started_pieces` and released by `on_disconnect`,
which is a plain method called from exactly one place: the end of
`session::run`. There is no destructor. Every other way out -- an early return,
a panic in the session task, cancellation at an await point -- kept a
piece-sized buffer per reserved piece, forever, and the piece was never
re-picked either. An engine accepting 76 inbound connections a second, a third
of them refused at the MSE handshake, takes those exits constantly.

`Drop` now calls `on_disconnect`, so ending the session is what releases the
pieces, not remembering to. The bug was never a missing call; it was that
correctness depended on reaching one.

`on_disconnect` had to become idempotent to be called twice: `started_pieces` is
drained, but `remove_bitfield` decrements availability counters, and running it
twice would understate the rarity of every piece the peer held. The bitfield is
cleared once removed.

## v4.4.5 -- the announce answer nobody wrote down

Four symptoms, one cause: seeders and leechers reading 0 on all 300k torrents,
"last announce" and "next announce" showing a dash, every tracker reporting
Success while the log filled with refusals, and the header's peer ratio stuck at
exactly 100.0%.

`TorrentState` carries `scrape_seeders`, `scrape_leechers`, `last_announce_at`,
`next_announce_at`, `last_announce_ok` and `last_announce_error`. Every reader in
the process consults them -- the detail panel, the list rows, the qBit shim.
Nothing has ever written them, in 3.x or 4.x: they were filled by the Go front,
which owned the announce loop. 4.0.0 moved that loop into `hydra/announce/` and
recorded each answer in `cache`, which no reader consults. Both halves worked;
they were not connected.

The runner now publishes onto the torrent as well as the cache. `last_announce_at`
moves only on success, as its doc comment always said it should -- a fresh
timestamp next to an error would be the wrong reading. Errors are redacted the
same way the log redacts them: the raw message embeds the announce URL, and that
URL carries the passkey.

The header ratio is separate and was worse: `swarm_leechers` and `unseeded_peers`
were served as the same value, so the UI divided a number by itself. The
denominator now comes from what the trackers actually report, summed in the
announce cache as a running total (`record` has the displaced entry in hand, so
it costs nothing and needs no periodic recount over 300k entries).

## v4.4.4 -- the dial that never gave up

`peer/handshake.rs` contains no timeout of any kind, and never has -- the file
is byte-identical in 3.x. `outgoing()` sends our 68-byte handshake and then
calls `read_exact` on the reply with nothing bounding it. A peer that accepts
the TCP connection and then says nothing holds that socket ESTABLISHED for the
life of the process.

Found on production while diagnosing something else: 3960 sockets to one peer,
every one showing `bytes_sent:68  data_segs_out:1` and no traffic for 47
minutes, accumulating at about 85 per minute. The accept path was bounded long
ago and its comment describes this exact failure from the other side -- *"a peer
that connects and then says nothing used to park a task in read_exact forever
[...] 20758 established sockets for 10874 peers"*. The fix was applied inline in
`peer/mod.rs` and never reached the four dial sites.

All four combos in `open_peer` (TCP/plain, TCP/MSE, uTP/plain, uTP/MSE) now use
the same 30 s the accept path allows, and `dial_hs_timed_out` counts what they
drop, so the cost of the leak stays measurable after it stops.

This is not a 4.x regression: 3.x leaked these sockets too. It does not explain
the upload collapse dated to the 4.0.0 cutover, and it was deliberately kept out
of 4.4.3 so the peer table would measure the swarm as it stood. Bundled here to
spend one restart instead of two.

## v4.4.3 -- the peer table, reconnected

`rpc::dispatch::get_peers` computes everything the peer panel needs -- per-peer
`interested`, `choked`, `is_seed`, the `iUEFS` flag string, rates and connection
age. 4.0.0 shipped it orphaned: no HTTP route reaches it, and the detail payload
hard-codes `"peers": []`.

That left the node with no way to answer the one question a stalled upload
turns on. Measured on production the same day: 87% of inbound peers (706 of 814)
received under 10 KB -- handshake, bitfield or HaveAll, unchoke, then nothing.
A peer that has been told we hold every piece and stays silent is either
interested and choked by us, or a seed with nothing to ask for. Those two have
opposite fixes, and nothing on the running node could tell them apart --
`unseeded_peers` cannot help, it is `active_peers` under another name.

Splits the body into `peers_json()` and fills the detail payload from it. No
behaviour change to the engine: this reads state that was already maintained.

## v4.4.2 -- the allocator the port left behind

`#[global_allocator]` is a per-binary attribute. 4.0.0 merged the Go front and
the Rust engine into one process by writing a new `src/hydra/main.rs` beside the
existing `src/main.rs`, and the attribute stayed on the old one. Every 4.x
release up to 4.4.1 therefore ran on the glibc allocator, while the Dockerfile
and the container environment both still set `MALLOC_CONF` -- which glibc
ignores. Nothing warned; the binary simply had no jemalloc in it.

glibc gives each thread its own arena, up to `8 x nproc`, and never returns a
secondary arena's pages to the kernel: there is no equivalent of
`dirty_decay_ms`. On the 300k-torrent production node that meant 728 arenas
across 630 threads and an RSS that did not stop climbing. Measured: 3.x held a
flat 14.4 GiB for 36 hours at the same torrent count; 4.x reached 100 GiB in
seven hours and had to be restarted.

Restores the allocator, and with it the SIGUSR1 heap dump, the SIGUSR2 decay
purge and the five-minute `allocated/active/resident/mapped/retained` line --
the instruments that separate a real leak from pages the allocator is holding.
They are shared between the two binaries now, in `src/hydra/allocdiag.rs`.

## v4.3.3 -- the exit IP refresh button

Clicking it left random digits in the header. The button scrambles the text
while it measures, and the page only renders the process address back when NO
engine reports an exit of its own -- a guard that was never true before 4.1.3,
because the per-engine measurement did not exist. It exists now, but
`/api/network/engines` still answered `"exits": []`, so the other renderer had
nothing to draw either. Both paths declined, and the animation was left as the
finished result.

- `exits` is now the DISTINCT set of local engine exit addresses, and
  `exit_ip_v6` is the v6 belonging to that same engine rather than the
  process's. One exit means the header can print it; several mean it cannot,
  and it says how many instead.
- `stopIpScramble()` restores what the field held before it started. Whoever
  has a fresh address overwrites it a moment later, but no path can leave the
  animation on screen as the final state again.

## v4.3.2 -- three the interface was showing wrong

- **Every header counter froze after its first paint.** The stream sent its
  status frame as `"status"`; the page dispatches on `"status_snapshot"` and
  ignores anything else. The frames were arriving on time, twice a second, and
  being dropped. Painted once by the direct /api/status fetch, then still.
- **The Queued chip counted zero** while rows beside it read "queued". The
  paging projection used the engine's RAW state, but a row's state comes from
  `derive_state`, which turns paused/stopped/queued into "stopped" or "queued"
  depending on whether the USER stopped it -- a flag that lives in the store.
  The projection derives it the same way now.
- **Tags rendered as `["cross-seed"]`, `["upload"]` and `cross-seed`**, three
  chips for two tags. The `tags` column is comma-separated except in rows an
  earlier importer wrote as a JSON array literal. Normalised on READ, so every
  consumer agrees and the stored data is left alone.

## v4.3.0 -- the list is paged end to end

The interface now reads its hoard list from `/api/hoard/page` instead of
receiving the whole library over the event stream. Sorting, filtering, search
and the facet counts all happen next to the data.

- The stream takes `hydrate=0` and carries only the live half: status, per-row
  stats, adds and removes. It still sends one empty terminal batch per mode,
  because the page waits for `done` before it stops showing the list as loading.
- Sorting a column, changing a filter chip and typing in the search box each
  refetch (search debounced 250 ms, answers applied in order so a slow early
  request cannot overwrite a newer one).
- **Facet counts are computed server-side**, each group counted with the OTHER
  groups applied and never its own -- the semantics the chips had when they
  could see the whole library. Counting the page instead would have labelled
  every chip with a number bounded by the page size: "All 500" on 300k, and
  category chips appearing and vanishing with the sort.
- Ctrl+A asks for `fields=hash`: the selection universe is every torrent the
  filter matches, and that is not what the page holds.

## v4.2.6 -- the pinned list stops reading the whole table

`/api/hoard/pinned` took six seconds to answer with an empty list. The covering
index carries `paused` but not `pinned`, so the query fell back to the table --
4.7 GB of .torrent BLOBs walked to read one flag per row. Same shape as the
21-second hydration the covering index was added for, one column short.

Fixed with a PARTIAL index (`WHERE pinned <> 0`), which holds only the rows
that are actually pinned -- a handful, usually none. Additive and invisible to
3.x, like the covering index: a rollback reads the same database and never uses
it. 6.35 s -> 0.9 ms.

## v4.2.5 -- the Records request leaves at once

4.2.4 made `/api/benchmark/records` answer in a millisecond, and the card still
took four seconds to appear. The wait had moved: the request was chained behind
`await api("/api/status")` inside `updateOverview()`, so it only left the
browser once that had resolved -- by which time `poll()` and the event stream
had taken the connections, and it queued behind a stream that never ends.

It is now fired first and on its own, and started before the await rather than
after it, which also means a failing `/api/status` no longer skips it. It
self-throttles to one call a minute, so the call `updateOverview()` still makes
costs nothing.

## v4.2.3 -- the Records card no longer holds up the header

`/api/benchmark/records` took 5.4 to 5.9 seconds, every call. It reads every
row of `bench_samples` -- 1.7 million of them -- and the overview header does
not paint until that request returns, so the whole page waited on it.

3.x served a cached copy and refreshed it in the background; the port carried
over the computation and not the cache. Restored: same 30 minute TTL, refreshed
off the request path, and warmed at startup so the first load of a new process
does not pay for it either. The refresh opens its own READ-ONLY handle, so the
five second scan cannot queue the sampler writing every five seconds behind it.

A failed scan keeps the previous answer: an empty Records card is worse than
one that is half an hour old.

## v4.2.0 -- server-side paging

`/api/hoard/page` and `/api/race/page` filter, sort and slice on the server:
`?offset&limit&sort&order&search&category&tag&tracker&state`, answering
`{total, filtered, offset, limit, rows}`.

The page already drew only its top 500 rows. The cost was never the rendering,
it was shipping the other 299500 to the browser so it could work out which 500
those were -- 250 MB per hard refresh. That decision now happens next to the
data, over a flat projection (`project_for_list`) that reads atomics instead of
building a forty-field JSON value per torrent; full rows are built for the page
alone.

Filter and sort semantics mirror `_hoardMatches` and `_hoardCmp` in app.js
exactly, tie-break on info_hash included: a page boundary that ordered ties
differently from the client would duplicate or drop rows between pages, which
reads as data loss. Store facts are only read up front when the FILTER needs
them (category, tag, pinned); otherwise the page's own hashes are looked up at
the end.

## v4.1.3 -- the version label, and a header that does not wait for the library

- `/health` was missing from the main router: only the rescue surface had one,
  so it answered 404. The page fills BOTH version labels from it, which is why
  the header showed none and the footer sat on its hardcoded `v0.1.0`. Two
  visible bugs, one absent route.
- The event stream sent the whole library before its first status frame. At 300k
  torrents that is seconds of a blank header on every hard refresh, while the
  same figures were available in under a hundred milliseconds. Status and the
  hoard header now go out first, hydration second.

## v4.1.2 -- "today" is a day again, not a lifetime

`day_uploaded` read 321 TB. The engines' per-torrent counters are LIFETIME
totals loaded from resume data -- they do not reset at boot -- and the port
published them straight into the fields labelled `session_*` and `day_*`. Every
one of the three figures was the same number wearing a different name.

- `session_*` is now the delta from a mark taken once the engines have loaded,
  so it starts at zero each boot.
- `day_*` is the delta from a baseline re-taken at the first local midnight
  after it was set, on a one-minute ticker rather than on the next request:
  3.x only rolled on a GET, so a dashboard opened in the afternoon showed
  yesterday's baseline until that moment.
- `global_*` still uses the lifetime totals. That column feeds the petabyte
  milestones, and subtracting this boot's mark from it would walk them backwards
  at every restart.
- Removing a torrent takes its lifetime bytes out of the sum, which can drop the
  total below the mark. The marks follow it down instead of publishing a
  negative day.

## v4.1.1 -- deleting a torrent reaches the engine

`DELETE /api/torrents/{hash}` dropped the store row and stopped there. The
torrent kept seeding and announcing, invisible to the interface, and came back
at the next restart from the engine's own state -- a ghost with a live socket.
It also ignored `delete_files` entirely, so a delete-with-data answered "ok"
and kept every byte.

⚠ The two sides disagree on polarity: the API asks whether to DELETE the files,
the engine is told whether to KEEP them. This is the direction that costs data
if it is passed through unflipped, so it is flipped explicitly and commented
where it happens.

## v4.1.0 -- torrents can be added again

Both add routes shipped in 4.0.0 as validation-only: they ignored their body
and refused everything. Nothing could reach this node -- autobrr had been
logging `unexpected status code: 400` for every release since the switch.

- `/api/v2/torrents/add` parses its multipart body and honours `category`,
  `savepath`, `tags`, `paused` and `skip_checking`, answering `Ok.`/`Fails.`
  the way qBit does, because the clients match on that body.
- `/api/torrents/add` accepts a `torrent_path` on this node. `magnet_uri` is
  still refused: resolution is a background job with its own polling contract,
  and answering "added" for metadata that never arrives is worse than a refusal.
- An add with neither a savepath nor a known category is refused rather than
  placed somewhere inferred. There is no correct guess, and the wrong one puts
  a download where its owner will not look for it.
- Adding writes all three things that have to agree: the .torrent where the
  engine's resume records point (by rename, never half-written), the engine's
  own state, and the store row the interface lists from.

## v4.0.2 -- records, milestones and announce rates

- The Records card and the milestone list render the fields the page actually
  reads (`date`, `unit`, `observed`, `since_prev`) and reuse 3.x's clean-period
  rule, which ignores everything before the last lineage jump in the lifetime
  counter. Publishing the raw peaks would have credited a counter change as the
  best upload day the node ever had, and marked every petabyte as pre-Hydra.
- The announce-rate graph has counters behind it again: each engine counts its
  own successful and failed announces, and the sampler differences them.

## v4.0.1 -- the interface reports what the engine is doing

4.0.0 moved the API into the engine's process, and a set of routes were left
answering a literal zero or an empty list until their slice was ported. On a
node seeding at 300 Mbit/s across a thousand peers, that is not a rough edge:
every live counter in the interface read zero, and nothing said why.

- **Live counters.** `/api/status`, `/api/hoard/stats` and
  `/api/benchmark/current` published hardcoded zeros for every rate, peer and
  swarm figure. They now read gauges the engine maintains. The gauges
  themselves are new: `update_rates` already walked the torrent map every two
  seconds, so counting peers and uploading torrents in the same pass costs
  nothing.
- **The list never updated.** After hydration the event stream sent only a
  status frame every two seconds, so every row kept the figures it was painted
  with. Each engine's delta-filtered stats emitter is now bridged to the
  stream. That emitter skips its whole scan while nobody subscribes, which is
  why nothing was being computed either.
- **The detail panel was empty for hoard.** `/api/hoard/torrents/{hash}`
  answered `{"status":"ok"}`. It now serves the same payload as race, from one
  shared builder so the two cannot drift again.
- **The trackers tab showed one row.** It listed only hosts carrying a client
  override. It now merges those with the trackers torrents actually announce
  to, with per-tracker counts and the last answer's time.
- **The benchmark tab was empty.** Nothing had written `bench_samples` since
  the 4.0.0 switch, and the range and records routes were stubs. A sampler
  writes the same five-second rows 3.x did, and the graphs, records and
  petabyte milestones read them.
- **No exit address anywhere.** Nothing ever filled the public IP cache. It is
  measured again, per engine through that engine's own binding -- an engine
  whose probe does not answer reports no address rather than borrowing the
  default route's, which is the whole point of the measurement.
- **Two O(N) paths on a polled route.** `session_totals` serialised all 300k
  torrents to JSON to add two integers, and `find_torrent` did the same to
  compare one hash. Both read the counters and the index directly now.

## v4.0.0 -- one process, one language

Hydra 3.x ran as two processes: a Go front that served the HTTP API, and the
Rust engine that owned the torrents, talking over a unix socket. The front kept
its own copy of every torrent's state to answer requests from. At 243k torrents
that copy was 1.62 GiB of live Go heap, 3.88 GiB of RSS all told, and it grew
by 6.6 KB with every torrent added -- a slope that does not reach a million.

4.0.0 is one Rust binary. The engines run inside it, and a request handler
reads the engine's own map. There is no second copy to keep in step, so the
class of bug where the UI showed a stale figure because a refresh had not run
yet cannot be written any more.

**This is a breaking change in packaging, not in the API.** Every route answers
byte-for-byte what 3.180.0 answered, including four bugs reproduced on purpose
and marked in place. The config file is unchanged. The SQLite schema is
unchanged, and a 4.0.0 that finds a 3.x database opens it as it is.

- The image ships one binary. `hydra-engine` is gone, and so is the unix socket
  between the two halves.
- `net = false` on an engine loads its state and opens no socket. It exists for
  benches that hold a real catalogue and must not announce it.

### How it was verified

Not by reading the Go. Every route was sent the same request as 3.180.0 and the
two answers compared down to the raw bytes, and every mutation was replayed
against both stores. 169 routes, 114 write mutations. Several contracts here
are not visible in the source and were found this way.

### Under it: state that assumed one engine per process

The engine library was written when one engine meant one process, and Hydra 4
puts two in one. Roughly thirty pieces of per-engine state lived in globals,
where the last engine to start decided for both -- silently, since none of it
is an error. They now belong to the engine that owns them: the DHT node, the
webseed queues, magnet jobs, the event bus, the completion channel, PEX, IPv6,
the dial ceilings, and the MSE block.

Two of those were not tidiness. A process-wide bind device meant the second
engine kept the first one's, or none, and its traffic left by the default route
-- the home address at the tracker, with no error and no log. A shared DHT
would have undone `enable_dht = false` on a hoard, which is what keeps 240k
idle torrents from paying for peer discovery they never use.

Two globals stay global, with the reason written where they live: the set of
our own addresses (an address belonging to either engine belongs to this host,
and skipping it is the safe direction) and the in-flight uTP dial set (keyed by
peer address, over a socket bound once per process).

## Release overview: v3.160.0 to v3.172.0

`main` last carried v3.160.0. This release lands twelve versions at once. The
per-version entries below are the record; this section is the map through them.

Almost all of it comes from running one instance at **192k torrents**, which is
where costs that look like constants stop being constants. Several of these
were not slow paths but *unbounded* ones: a buffer that only ever grew, a span
that was never dropped, a socket nothing reaped.

### Memory, on a large catalogue
- **A `get_peers` tracing span was held open for every public torrent, forever**
  (v3.161.0). At ERROR level the `info` filter never discarded it, so the
  subscriber registry accumulated one root span per public torrent for the life
  of the process: 7.51 -> 10.18 GiB in 38 minutes.
- **A complete torrent no longer allocates a piece picker** (v3.162.0). 82954
  torrents held one for nothing; `PiecePicker::new` was 21% of the engine's live
  heap. `PiecePicker::availability` also went from `Vec<u32>` to `Vec<u16>`.
- **A peer connection kept a 128 KiB read and write buffer for life**
  (v3.169.0). `BytesMut` grows by doubling and could not reclaim in place, so a
  connection that once buffered a 16 KiB `Piece` ratcheted up and stayed there.
  2042 live buffers averaged ~131 KB on connections whose steady state is
  17-byte `Request` messages.
- **The allocator's decay dial is movable at runtime** (v3.170.0).
  `dirty_decay_ms`/`muzzy_decay_ms` are the RAM/CPU trade-off here -- forcing
  them to 0 took RSS from 6.81 to 2.86 GiB at equal age -- but they were set
  through `MALLOC_CONF`, so retuning them meant recreating the container. They
  now move on `SIGUSR2`, and the jemalloc line reports `metadata` so allocator
  bookkeeping stops being mistaken for application memory.

### Descriptors and connections
- **An inbound peer that connected and then said nothing leaked its socket
  forever** (v3.163.0). No deadline on the handshake read, and no `PeerGuard`
  built yet, so nothing counted it and nothing reaped it: 20758 established
  sockets for 10874 peers, growing ~9500 fd/h. Every step of the inbound
  handshake is now bounded, and `HS_TIMED_OUT` names the cause. This also closed
  a trivial resource exhaustion.
- **A seeding torrent no longer asks the tracker for peers, and two seeders no
  longer stay connected** (v3.164.0). `numwant=200` on every one of ~192k
  seeding torrents meant one dial per shared swarm: 21372 sockets across 4163
  destinations, 8392 of them to a single peer, 20986 with nothing queued either
  way. `numwant` is 0 when `left == 0`, and a session where both sides are
  complete closes. Note that this makes a complete torrent **passive**: it now
  depends on the listen port staying reachable.

### Interface
- **The whole UI answered about a second late** (v3.171.0) -- hover, cursor
  shape, clicks. Each `stats_snapshot` frame sorted all 196k torrents to paint
  500 rows, and the comparator's tie-break ran an Intl `localeCompare` on
  40-character info hashes for nearly every comparison. Top-K through a bounded
  heap, same rows in the same order: 130ms -> 13ms by date added, 360ms -> 18ms
  by upload rate, 340ms -> 41ms by name.

### Configuration
- **DHT and PEX can be turned off, per engine** (v3.172.0). `enable_dht` and
  `enable_pex` in `[race]` and `[hoard]`, both defaulting to true, both reaching
  a remote node's engines through the pushed agent config. For the operator who
  wants a seedbox that talks to nothing but its trackers.

### Versions 3.165 to 3.168
Never released. Those numbers were spent on a line of work that did not reach
this branch, so the history goes from 3.164.0 straight to 3.169.0.

## v3.180.0

### Fixed
- **Webseed fetches now use HTTP/1.1, and that is the whole performance story.**
  archive.org negotiates h2 (ALPN verified), so reqwest was multiplexing every
  concurrent request to a host onto a single TCP connection whose bandwidth the
  streams then shared. The phase instrumentation added in v3.179.0 made it
  unambiguous: 9.25 requests per span taking 11.2 s at 1.21 s each -- exactly
  their sum, so no overlap whatsoever -- with 761 s of a 60 s window spent in
  fetch and essentially nothing anywhere else. That is why raising the worker
  count from 16 to 48, batching pieces into spans, issuing a span's file
  requests together and moving the slot budget between 300 and 25000 had all
  left the rate pinned around 2.7 MB/s: they were adding parallelism upstream
  of a transport that refused to carry it.
  A plain HTTP/1.1 benchmark run from the same host, on the same IP, at the same
  moment, pulled 20.78 MB/s -- it opens one socket per thread. Forcing h1 gives
  each in-flight request its own connection, and `pool_max_idle_per_host` is
  raised to 256 so those connections are reused instead of re-handshaking TLS
  at every request.

## v3.179.0

### Added
- **The webseed loop now reports where its wall clock goes.** Six hypotheses
  had been tested and refuted by measurement — origin throttling (a 32-stream
  benchmark pulled 20.78 MB/s from archive.org *while* the engine crawled),
  worker count (16 -> 48 changed nothing), span truncation, per-file
  serialisation, catalogue scanning, and the download-slot budget (300, 2000,
  8000 and 25000 slots all landed between 2.0 and 2.8 MB/s) — and the rate
  stayed flat throughout. Rather than a seventh guess, each worker now
  accumulates the nanoseconds it spends waiting for work, picking a run,
  fetching, feeding the picker and committing, and one line a minute prints the
  breakdown with requests/s, MB/s and ms per request. Counters are swapped to
  zero on every report, because a running average hides a regime change.

## v3.178.0

### Fixed
- **Webseed workers spent their time walking the catalogue instead of
  downloading.** Each worker called `find_torrent` — a walk of all 243k
  torrents — **once per torrent it claimed**, and for each of the ~2000 in
  download state it took that torrent's picker mutex for an `is_complete()` in
  O(pieces). With the worker count raised to 48 the walk became the engine's
  main occupation: CPU went from 87% to 223% while throughput stayed exactly
  where it was, at ~2.7 MB/s.
  Proof that the origin was not the limit: a plain 32-stream python benchmark
  run **at the same time, from the same host and IP**, pulled **20.78 MB/s**
  from archive.org while Typhon crawled at 2.7. Three successive increases in
  concurrency had produced no gain at all, which is the signature of spinning,
  not of an external cap.
  A single scanner task now refills a shared queue in one walk per batch, on a
  blocking thread rather than on a runtime worker, and the workers just pop
  from it. `TorrentManager::collect_torrents` gathers a batch in one pass and
  returns info hashes only, so the result keeps nothing alive.


### Fixed
- **Webseed spans were being truncated by a random starting piece.** The pool
  asked the picker for work through `pick_piece`, which is rarest-first with a
  random tie-break — correct for a swarm, wrong for an HTTP mirror, where every
  piece has availability 0 and the tie-break therefore picks uniformly at
  random. Since a run can only be extended *forwards*, starting in the middle
  of a five-piece torrent produced a two-piece span touching two or three
  files, so `buffered(6)` had almost nothing to overlap. Measured in
  production: **~14 requests actually in flight where the design allows 96**,
  while archive.org answered in 1.68 s median under that same load and the
  engine sat at 0.88 core with a flat profile — neither the origin nor the CPU
  was the limit. The pool now takes `PiecePicker::first_missing`, the
  lowest-indexed piece that is neither held nor reserved, so runs are as long
  as the torrent allows and the common case is one maximal span per torrent.

### Changed
- `webseed_max_concurrent` default 16 -> 48. The download-slot manager opens
  2000 slots; with 16 workers the other ~1984 torrents made no progress between
  its 30 s ticks, so it demoted them into cooldown exactly as designed, which
  then starved the pool of candidates.
- The pool's idle wait when no candidate is available drops from 10 s to 2 s.
  Ten seconds was pure lost throughput whenever the candidate set thinned.


### Changed
- **A webseed span's per-file requests now go out together instead of one after
  another.** v3.174.0 grouped contiguous pieces into one 8 MB span and lifted
  production from 0.80 to 2.07 MB/s — real, but far short of the 51 MB/s the
  bench had shown at the same concurrency. The remaining cost was not the piece
  grouping at all: BEP 19 gives **one URL per file** and a byte range cannot
  straddle two of them, so a 2.2 MB Internet Archive item spread over its
  median **10 files** costs ten requests however the pieces are batched.
  Measured on the running catalogue: 1981 torrents in flight held 19 953 files,
  **10.1 files per torrent at 179 KB each**. Ten sequential round trips of
  ~2.5 s each gives 0.089 MB/s per worker, and 1.4 MB/s across the 16 of them —
  which is the 2.07 MB/s that was actually observed. The model and the
  measurement agree, so the fix is to overlap the requests, not to add workers.
  A span's file requests are now issued with `buffered(SPAN_FILE_PARALLEL)`,
  which preserves stream order so the pieces can still be cut back out of the
  assembled buffer. The parallelism is 6, keeping the engine-wide ceiling near
  96 in-flight requests at the default 16 workers — the neighbourhood of the
  32-stream bench that reached 51 MB/s without finding a limit, rather than a
  leap past anything measured.
  The per-file range arithmetic moved into `span_file_ranges`, a pure function,
  because that is where an off-by-one silently assembles corrupt bytes that
  only surface later as a SHA1 failure.


### Fixed
- **A torrent whose data has no folder of its own can change category again.**
  When a payload sits directly in a category directory -- the shape an
  rtorrent or Transmission import leaves behind -- its content root *is* that
  directory. The mover is expressed as directory-to-directory, so moving the
  torrent would have moved every other torrent in the category with it, and
  the only safe answer available was to refuse: `0 OK, 1 failure` with no way
  forward.
  Such a payload now moves file by file. The file list comes from the torrent
  rather than from walking the directory -- that directory belongs to the
  category, and walking it would sweep up everything in it -- and the source
  directory is never renamed and never removed. The layout is preserved on the
  other side: the files land loose in the target category directory, because
  changing a category should not silently restructure somebody's library.
  Four things this path has to do that the ordinary one does not:
  collisions are checked **per file**, since the target category is
  legitimately non-empty and a name already taken belongs to another torrent
  (a refusal, never an overwrite, re-checked at swap time because a plan is a
  snapshot); a rename that fails part way **puts back what it already moved**,
  half a payload in each directory being the one outcome with no good
  recovery; `stat` reporting one filesystem while `rename` returns `EXDEV` is
  the ordinary shape of two bind mounts of one pool, so the move falls back to
  a copy, restarting the torrent first through a new `AbortSwap` hook that
  undoes the stop without repointing the engine at data that has not moved
  yet; and paths coming from the torrent are rejected if they are absolute or
  climb out of the directory, a torrent being remote input.
  A mount point is accepted as a loose source, unlike elsewhere in the mover,
  precisely because the directory itself never moves.
  Remote agents still refuse this layout -- that mover is a separate
  implementation.

## v3.174.0

### Changed
- **Webseed fetches a run of contiguous pieces per request instead of one
  piece.** v3.173.0 worked but ran at **1.12 MB/s in production** against a
  benchmark that had shown 51 MB/s, and the gap was entirely round trips:
  archive.org costs ~2.5 s of latency per request whatever its size, so a
  512 KB Internet Archive piece spent about 80% of its life waiting for
  headers.
  A worker now reserves pieces forward from the one the picker chose until it
  has about **8 MB** in hand, and pulls the lot with a single ranged GET per
  file the span touches. The target is a byte budget rather than a piece count
  on purpose: `piece_length` ranges from 16 KB to 16 MB across torrents, so
  counting pieces would produce absurd request sizes at both ends. 8 MB is
  where transfer time comes back to the same order as the latency (~50%
  efficiency against ~17%) while 16 workers still hold at most 128 MB of
  buffers between them; past that the curve flattens and only the memory grows.
  Since the median Internet Archive item is 2.2 MB, the common case collapses
  to **one request for the whole torrent**.
  The run stops at any piece already held or already reserved, so batching
  never resets a piece another source is part-way through delivering — that is
  what the new `PiecePicker::is_pending` is for. Each piece is still cut back
  out of the span and committed individually, through the same block-level
  alignment checks and the same completion sequence as before.

## v3.173.0

### Added
- **BEP 19 webseeds (`url-list`): a torrent published without a seeder can now
  complete.** Some publishers ship torrents that were never meant to have a
  swarm. The tracker only helps downloaders find each other; the bytes live on
  an HTTP mirror named in the `url-list` key. Every one of Internet Archive's
  ~88M items is built that way. Typhon parsed `announce-list` and ignored
  `url-list` entirely, so such a torrent sat at 0% for ever with nothing to
  explain it: across a 51,158-torrent sample, 100% had zero seeds and **zero
  bytes received**, and not one error was logged.
  A fixed pool of workers (`webseed_max_concurrent`, default 16) claims a
  torrent, pulls its pieces with ranged HTTP GETs and releases it. A pool
  rather than a task per torrent, because a million-torrent catalogue cannot
  afford a task each, and the origin bounds throughput long before the
  catalogue does (measured against archive.org: 2.4 MB/s at one stream,
  51 MB/s at 32, still climbing).
  A fetched piece is handed to the picker in 16 KiB blocks, so it passes the
  same alignment and length checks as anything arriving from a peer, and is
  stored through the same completion sequence the peer path uses.
  Per engine, `enable_webseed`, on by default.
  A torrent that fails five times running is parked for an hour: a deleted or
  renamed item answers 404 for ever, and at catalogue scale that must not turn
  into a permanent retry storm against the mirror.
  A `Range` request answered with 200 instead of 206 is refused unless the
  range happened to cover the whole file, so a server that ignores `Range`
  cannot make a 512 KB piece pull a multi-gigabyte body.
- ⚠️ An engine pinned to a device (`bind_interface`) with no
  `TYPHON_ANNOUNCE_PROXY` set does **not** start the pool. reqwest opens its
  own sockets, so the `SO_BINDTODEVICE` pin the engine applies to its peer
  sockets does not reach them, and the GET would leave by the default route
  carrying the host address. Refusing to fetch is the only safe answer.

### Changed
- **Piece completion now exists in exactly one place.** The peer path and the
  webseed pool share `commit_piece`: SHA1 through the disk layer, `set_have`,
  the Have broadcast, the hash-table release and the durable completion
  notice. This is deliberate — the last time a completion path diverged from
  what the resume writer expected, every complete torrent had its bitfield
  erased on the following sweep.

## v3.172.2

### Fixed
- **A torrent was stopped to free a download slot nobody was waiting for.**
  Activity demotion exists to hand a *scarce* slot to a torrent that can use it,
  but `enforceDownloadSlots` ran it whatever the pressure. With
  `max_downloads=2000` and 114 incomplete torrents, one tick logged
  `demoted=89` and the next `started=104` while **1886 slots sat idle**: a full
  stop -> cooldown -> re-announce -> reconnect cycle that freed nothing.
  Progress is now only judged when slots are actually contended
  (`incomplete > maxSlots`). A torrent idling while slots are free keeps its
  slot *and* restarts its evaluation window, so if contention does appear it is
  judged on fresh evidence instead of being demoted the instant it appears, on
  evidence gathered while stalling cost nothing.
- **The Go announcer still asked for 200 peers on a completed torrent.**
  v3.164.0 set `numwant=0` when `left == 0` in the Rust announcer but left the
  Go path unconditional, so half that fix was missing and the hoard kept asking
  trackers for peers it had no reason to dial. Both paths agree now.

## v3.172.1

### Fixed
- **v3.170.0 published a tree that does not compile.** Its `main.rs` called
  `torrent::take_completion_receiver` and `TorrentManager::persist_completed`,
  neither of which had been committed: every Rust job of v3.172.0 -- the release
  binaries, the container image and the CI check -- failed on those two errors,
  while the Go side passed. The sources are in the tree now. They are the engine
  that has been running in production since 2026-09-03; only the commit was
  missing. What they contain:
  - **A complete torrent was written back to the store with an empty bitfield,
    and came back at 0% on the next boot.** Since v3.162.0 a torrent loaded
    already-complete gets no piece picker, but `build_resume_data` still read
    the bitfield off that picker, and with no picker it wrote `""`.
    `bitfield_is_complete("")` is false, so the next start rebuilt the torrent
    at zero, allocated an all-missing picker and re-downloaded data already
    sitting on disk. `last_saved` is empty at boot, so the first sweep marked
    the whole catalogue dirty and did this to all of it.
    `build_resume_data` now tells apart the two ways a torrent can lack a
    picker: `seed_mode`, where an empty bitfield is the intended encoding and
    the torrent is trusted complete on load, and loaded-complete, where it
    writes the full bitfield the picker would have produced.
  - A completion channel (`notify_completed` / `take_completion_receiver`), so a
    torrent finishing mid-session persists its record immediately instead of
    waiting for the periodic sweep.
  - The `seed_seed_dropped` counter that the v3.164.0 entry already described.

## v3.172.0

### Added
- `enable_dht` and `enable_pex`, per engine, in `[race]` and `[hoard]`. Both
  default to true, which is what every install has run so far, and both appear
  in the Configuration tab beside `enable_ipv6` rather than behind the advanced
  toggle: someone seeding on a private tracker should not have to hunt for
  them. Existing configs gain the keys on the next start through the additive
  migration, so the editor shows them without hand-editing the file.
- `enable_dht = false` stops the engine bootstrapping a DHT node at all. No
  `get_peers` stream is opened, and every later `track_torrent` (add, start,
  resume, magnet resolution) turns into a no-op on its own.
- `enable_pex = false` stops advertising `ut_pex` in the BEP 10 handshake and
  drops the peer's ut_pex id, so an incoming PEX message is ignored rather than
  parsed. Advertising the extension and silently discarding what arrives would
  still have told the swarm we trade peer lists.
- Both settings reach a remote node's engines through the pushed agent config,
  so turning a peer source off is fleet-wide and not just local.

### Notes
- `private` torrents (BEP 27) were, and remain, excluded from both regardless
  of these keys.
- The `dht_enabled` / `pex_enabled` fields already existed in the engine config
  JSON, hardcoded to true and ignored by Typhon since the libtorrent era. They
  are now read, and carry the operator's choice.

## v3.171.0

### Fixed
- The whole UI answered about a second late: hovering a menu, the cursor shape
  and clicking a torrent all lagged behind the mouse. Every `stats_snapshot`
  frame (about one per second) made the hoard table sort all 196k torrents to
  paint 500 rows, and the comparator's tie-break ran an Intl `localeCompare` on
  40-character info hashes for nearly every comparison whenever the sorted
  column held mostly equal values. The table now takes the top 500 through a
  bounded heap and orders only those, for the same rows in the same order:
  130ms -> 13ms sorted by date added, 360ms -> 18ms by upload rate, 340ms ->
  41ms by name.
- The hoard table also rebuilt itself once a second while another tab was on
  screen. It now defers the repaint until the Hoard tab comes back.

## v3.170.0

### Added
- **The allocator's decay dial can be moved without recreating the container,
  and the stats line now reports `metadata`.** `dirty_decay_ms`/`muzzy_decay_ms`
  decide how long jemalloc holds freed pages before handing them back, and they
  are the RAM/CPU trade-off on this engine: on 2026-09-01, forcing them to 0
  took RSS from 6.81 GiB to 2.86 GiB at equal age, while `perf` put jemalloc at
  0.03% of CPU and `madvise` at 0.00%. They are set through `MALLOC_CONF`, an
  environment variable, so moving them meant recreating `hydra-go` -- the one
  operation here with a real blast radius -- and so they were never retuned.
  Both are writable at runtime, so `SIGUSR2` now sets them to 0 across every
  arena and logs `resident` either side. `allocated` should not move; if it
  does, the reading was not what we thought it was.
- `metadata` in the five-minute jemalloc line. Without it, allocator bookkeeping
  was indistinguishable from application memory when `allocated`, RSS and the
  sampled profile disagreed -- which is exactly what happened on 2026-09-02,
  where 85 GiB of address space was mapped for 5.35 GiB resident.

## v3.169.0

### Fixed
- **A peer connection kept a 128 KiB read buffer for life.** `BtCodec` accepts
  frames up to 256 KiB, and `BytesMut::reserve` grows by doubling, so any
  connection that once had to buffer a 16 KiB `Piece` message ratcheted its
  read buffer up and never gave the memory back: `split_to` hands the payload
  out on the same allocation, so `bytes` cannot reclaim the space in place
  while that payload is alive. A 192k-torrent instance held **2042 live
  read/write buffers averaging ~131 KB** -- on connections whose steady state,
  once a torrent is seeding, is 17-byte `Request` messages.
  Both `decode` and `encode` now swap in a right-sized buffer when the existing
  one is **empty** and has grown past 64 KiB. Empty means there is nothing to
  copy, and the test is one comparison on a path that already returns early, so
  the hot path is untouched: a connection actively moving blocks never empties
  its buffer between frames and is left alone. The write half is the one that
  matters on a seeding instance -- it is the buffer carrying the 16 KiB Piece
  payloads -- so fixing only the read side would have left half the memory in
  place.

  Measured: the guard tests fail with `kept 131072 bytes` on both halves when
  the swap is removed, which is the ratchet reproduced in isolation.

## v3.164.0

### Fixed
- **A seeding torrent no longer asks the tracker for peers, and two seeders no
  longer stay connected.** Both announce paths sent `numwant=200`
  unconditionally, so every one of ~192k seeding torrents asked for 200 peers
  and dialed everything that came back. Because a BitTorrent connection belongs
  to one torrent, a large seedbox sharing thousands of swarms with us was dialed
  once per swarm: prod held **21372 established sockets across only 4163 distinct
  destinations** (5.13 each), with three addresses alone accounting for 76% --
  8392 simultaneous connections to a single peer. 20986 of those sockets had no
  data queued in either direction: seeder-to-seeder links with nothing to
  exchange. Descriptors grew ~9500/h against a 1M limit, scaling with the
  catalogue rather than settling.
  - `numwant` is now 0 when `left == 0`. We are directly reachable, so leechers
    open the connection to us; a complete torrent has nothing to dial.
  - A session that learns (via `Bitfield` or `HaveAll`) that the remote holds
    every piece closes when we are complete too. Completeness is re-read live,
    not taken from the snapshot made when the session opened.
  - New `seed_seed_dropped` counter in `/api/status`, next to `inbound_accepted`.

### Note
  This makes a complete torrent **passive**: it depends on the listen port
  staying reachable. If the port forward breaks, upload stops dead rather than
  degrading, and there are no outbound dials to compensate.

## v3.163.0

### Fixed
- **An inbound peer that connected and then said nothing leaked its socket
  forever.** `handle_incoming` read the handshake with `read_exact` and no
  deadline, so the task parked on the read, the socket stayed ESTABLISHED, and
  because no `PeerGuard` had been built yet nothing counted it and nothing
  reaped it. Prod showed **20758 established sockets for 10874 peers**, growing
  ~9500 fd/h against a 1M limit. Every read and write of the inbound handshake
  -- first byte, plaintext remainder, our reply, MSE Ya, and the whole MSE
  exchange -- is now bounded by a 30s timeout, and a `HS_TIMED_OUT` counter
  names the cause instead of leaving the fd curve unexplained.
  This also closed a trivial resource exhaustion: opening connections and
  never sending was enough to consume the engine's descriptors.

## v3.162.0

### Changed
- **A complete torrent no longer allocates a piece picker.** The resume loader
  built a `PiecePicker` for every torrent whose stored `seed_mode` was 0,
  imported the bitfield into it, concluded the torrent was complete, promoted it
  to Seeding -- and then kept the picker for the life of the process. On a
  192k-torrent instance, 82954 torrents were in exactly that state and
  `PiecePicker::new` was 21% of the engine's live heap. Completeness is now
  decided from the resume bitfield *before* the state is built
  (`bitfield_is_complete`), so those torrents get no picker at all.
  A partially-downloaded torrent is unaffected and still gets one.

## v3.161.0

### Fixed
- **DHT: a `get_peers` span was held open for every public torrent, forever.**
  `request_peers_forever` wrapped its task in `error_span!(parent: None, "get_peers", ...)`.
  At ERROR level the default `info` filter never discarded it, so the subscriber
  registry accumulated one root span -- plus a formatted `info_hash` String -- per
  public torrent, for the life of the process. The per-request `error_span!("addr", ...)`
  had the same problem, once per in-flight DHT request. Both are now `debug_span!`,
  which the `info` filter drops before any allocation happens. Measured on a
  192k-torrent instance: the engine grew 7.51 -> 10.18 GiB in 38 minutes
  (~4.2 GiB/h) before this change.

### Changed
- `PiecePicker::availability` is `Vec<u16>` instead of `Vec<u32>`: 2 bytes per piece
  instead of 4. A swarm never holds 65535 copies of a piece, and the increments
  now saturate instead of wrapping.

## v3.160.0 - 2026-08-29

### Fixed
- **An engine no longer dials itself over IPv6.** The self-dial filter only
  knew the addresses the Go side pushed to it, and in production that set held
  the public IPv4 alone. Every IPv6 address the host owns was therefore
  invisible to it, so the hoard kept connecting to its own listener: 7342 live
  self-connections were measured, about a quarter of everything the engine
  counted as a peer, each one a TCP connect plus a handshake thrown away and
  retried.

  Each engine now discovers its own addresses instead of waiting to be told,
  reading them from the interfaces at startup and every two minutes after (a
  tunnel raised later brings an address that must not be dialled either). The
  push from Go stays, but as a supplement carrying the address seen from
  outside, which cannot be observed locally -- it is no longer the only source.

  IPv6 only, deliberately: local IPv4 addresses are RFC1918, never routable and
  never handed back by a tracker as a peer, whereas every global IPv6 address
  is routable and can come back to us in a peer list. Link-local and loopback
  are skipped for the same reason.

- **An agent can still reach another agent.** The filter stays port-aware: our
  own address on our own listen port is us, the same address on a different
  port is the engine next door. Two copies of one torrent on two engines share
  bandwidth exactly as before -- only the loop back onto ourselves is cut.

- **The local IPv6 enumeration no longer sits behind `enable_ipv6`.** That gate
  is what failed: the flag did not reach the decision, so the whole local set
  was skipped. Listing local interfaces is a syscall with no network round
  trip, so there was nothing to gate. Only the public-IPv6 lookup, which does
  hit the network, still is.

### Added
- `self_dial_filter` in the engine diagnostics: the pushed set, the discovered
  set, and `dials_skipped_self`. That counter already existed and was published
  nowhere, which is why 7342 self-dials could accumulate without a single
  signal anywhere.

## v3.159.0 - 2026-08-29

### Changed
- **The peer session loop stopped rebuilding its timers on every message.**
  Both the 300s idle deadline and the 10s choke tick were constructed inside
  the loop body, so each turn allocated a `Sleep`, took tokio's timer-wheel
  lock to register an entry, and took it again on drop to cancel it. That lock
  (`tokio::runtime::time::Inner`) is a single mutex for the whole process; with
  67k live sessions spread over 12 worker threads, a DWARF profile put
  `parking_lot::RawMutex::lock_slow` under it at 17-19% of the engine, plus
  ~11% more in `Wheel::insert`/`remove`/`reregister`/`clear_entry`. Close to a
  third of the engine's CPU was timer bookkeeping rather than BitTorrent.

  Both timers are now created once per session and reset in place.

- **The 300s idle deadline now actually fires on downloading sessions.** It
  could not before: the deadline was rebuilt every turn of the loop, and the
  10s choke tick turned the loop, so a non-seeding session reset its own
  timeout every 10 seconds and never reached it. Seeding sessions are
  unaffected -- their choke arm is disabled, so only peer traffic ever turned
  their loop, which is exactly what the new reset condition is.

### Fixed
- Ephemeral-port exhaustion on the host was starving `connect()`: at 207k
  torrents the hoard held ~28k distinct source ports against a 28,232-port
  range (99% occupied), and `__inet6_check_established` plus the spinlock in
  `__inet_hash_connect` accounted for 20% of the engine's profile. This is a
  host sysctl rather than a code change (`net.ipv4.ip_local_port_range`), and
  it is recorded here because the dial-governor work above only makes sense
  read together with it.

## v3.158.0 - 2026-08-29

### Added
- **The outbound dial governor can be moved while the engine runs.**
  `max_dials_per_sec` and `max_connections` both existed, but neither could be
  changed without a restart: the rate was captured once when `DialPacer` was
  built, and the connection ceiling was seeded from the config at boot and
  never written again. Restarting a 200k-torrent hoard to try a value is not a
  knob anyone turns twice, so in practice both stayed at their default of
  unlimited. `POST /api/hoard/dial-limits` and `POST /api/race/dial-limits`
  now apply either ceiling on the next dial and persist it to the TOML, on the
  same pattern (and the same write lock) as the listen-port hot-rebind.

  Both fields are optional, and nil means "leave this one alone" rather than
  zero, because zero is itself a meaningful value: unlimited. A plain int could
  not tell the two apart and would silently un-cap the other ceiling.

  This exists because a hoard seeding 207k torrents was measured burning 4.7
  cores with 37% of its profile inside `connect()`: at 1600 outbound dials per
  second against a 28k-port ephemeral range that was 92% occupied, the kernel
  spends its time in `__inet6_check_established` walking hash chains under a
  spinlock. The dial rate is the multiplier on that cost, and it was the one
  input nothing could reach.

## v3.157.0 - 2026-08-29

### Added
- **The engine reports what the allocator is actually holding, every five
  minutes.** A sampled heap profile could not settle where the memory was
  going: even sampling every 4 KiB it accounted for under a quarter of the
  resident set, which left "live objects the sampler misses" and "pages
  jemalloc kept rather than returning" impossible to tell apart. The engine now
  logs jemalloc's own unsampled counters -- allocated, active, resident,
  mapped, retained -- plus the two differences worth naming: allocator slop
  inside live pages, and dirty pages not returned to the OS.
- **`typhon-engine/tools/heap_report.py`** turns a heap dump into a table of
  allocation sites. `jeprof` cannot: the engine is a PIE and jemalloc's dump
  lists no file-backed mapping, so every frame symbolises as `?`. The tool
  reads the load base from `/proc/<pid>/maps`, subtracts it, and batches the
  addresses through `addr2line`.

### Fixed
- **683 MB of broadcast channels for pieces that will never complete.** Every
  torrent not added in seed mode built a 256-slot have-broadcast ring, ~8.4 KiB,
  and held it for life. Production had 81,131 of them, nearly all on torrents
  that had long since finished downloading and would never announce another
  piece. The ring is now created on demand by the download path and dropped as
  soon as the torrent starts seeding, whether it got there by finishing a
  download, by a recheck, or by being restored complete at startup. Peers
  attaching to a seeding torrent no longer create one just by subscribing.

## v3.156.3 - 2026-08-28

### Fixed
- **A torrent that finished downloading kept its piece hashes for good.**
  v3.156.2 stopped loading the hash table until something needed to verify a
  piece, but once loaded it stayed for the life of the torrent. On a seedbox
  that is forever, so a recheck or a completed download quietly handed back the
  20 bytes per piece -- 20.1 KiB for the average torrent -- that the change
  existed to save. The table is now dropped the moment a torrent reaches
  Seeding, by either route, and reloaded from the `.torrent` if a later recheck
  needs it. A verification already running keeps the copy it started with.

## v3.156.2 - 2026-08-28

### Fixed
- **4.2 GB of RAM held piece hashes that nothing was reading.** A torrent's
  SHA-1 piece hashes are 20 bytes per piece and dominate a `.torrent` file --
  measured across 4000 production torrents, 91.7% of the bytes, averaging
  20.1 KiB each. The engine parsed them into memory on add and kept them there
  for the lifetime of the torrent, 4.2 GB across a 205k-torrent instance. Only
  two call sites ever read a hash: rechecking a torrent, and accepting a piece
  we just downloaded. Both already read the piece off disk and SHA-1 it, so one
  extra file read costs nothing next to the work they were doing anyway. A
  torrent that only seeds never verifies anything and now holds no hashes at
  all. The table is loaded from the `.torrent` on first use and kept from then
  on, which is the same trick the engine already used for the BEP 9 info dict.
  A torrent whose file has gone missing now refuses to verify rather than
  accepting the piece.

## v3.156.1 - 2026-08-28

### Fixed
- **3.3 GB of RAM went to per-torrent maps that never held anything.** Every
  torrent carries two concurrent maps for its peers. `DashMap::new()` sizes its
  shard array from the machine -- `(cores * 4)` rounded up to a power of two --
  and allocates the whole array at construction, before a single peer connects.
  On a 12-core box that was 64 shards of 128 bytes per map, so 16.7 KiB per
  torrent whether or not anything was ever inserted. Measured on a 204,893
  torrent instance, 167k of them with no peers at all: 3.43 GB of empty shards.
  These maps are per torrent, not global -- they are touched on peer connect and
  disconnect -- so the sharding bought nothing. They now use the minimum,
  dropping the cost to 512 bytes per torrent. The global maps that really are
  contended keep the default sharding.

## v3.156.0 - 2026-08-27

### Added
- **Hydra brings its own WireGuard tunnels up, one per engine.** Give an engine
  a provider `.conf` and say which provider it came from; Hydra creates the
  interface, loads the keys, routes it, and asks for a forwarded port. The
  engine is then born already correct: it binds the port the provider granted
  and announces it once, instead of announcing a guess and being unreachable
  for a full announce cycle while trackers hold on to it.

  `wg-quick` is never used, and that is the point. A provider config carries
  `AllowedIPs = 0.0.0.0/0`, which `wg-quick` reads as "make this the machine's
  default route" -- on a host that also serves a library or has to stay
  reachable, that is a change with no undo. The file is parsed, not executed:
  every route Hydra installs goes into a routing table of its own, consulted
  only for sockets explicitly bound to that tunnel device. Measured on the
  production host: `ip route` and `ip rule` are byte-for-byte identical before
  and after two tunnels come up and go down.

  Two tunnels on one host now genuinely leave by two addresses, which is the
  whole reason for per-engine tunnels: measured `203.0.113.21` and
  `203.0.113.22` from two Proton configs on the same machine, with each
  engine's NAT-PMP mapping landing on its own port (45243 and 61219).

- **It is a network mode of its own, the fifth**, not a corner of Direct. In
  every other mode the operator decides the egress: picks an interface, types a
  proxy address, and the page is a form. Here the operator hands over a
  provider file and the egress is a consequence -- the interface is created and
  named by Hydra, the listen port is chosen by the provider and rotates on its
  own. Leaving those two on screen as editable boxes under Direct meant showing
  fields whose contents are overwritten at every boot.
  Picking any other mode switches the tunnels off, the way picking a mode has
  always cleared the previous one's keys: a tunnel left enabled would keep
  being built at each boot under a page claiming the node is direct.

- **Port forwarding is asked for, and then followed.** The provider dropdown is
  not decoration: it is how Hydra knows whether there is a port to ask for at
  all. Proton and other NAT-PMP gateways are asked directly; AirVPN, PIA and
  Windscribe assign a port out of band, so the field is yours to fill; Mullvad
  removed forwarding in 2023, and the UI says so rather than leaving an engine
  silently unable to take incoming peers.

  A NAT-PMP lease is sixty seconds and nothing announces its expiry -- the port
  simply stops answering while the engine keeps advertising it. So it is renewed
  at half the lease, and a port that comes back different moves the engine's
  listener. Both TCP and UDP are mapped: forwarding only TCP works well enough
  to look correct and quietly loses every uTP and DHT peer.

- **Every engine is followed, not just the first two.** The gluetun follower
  only ever knew `race` and `hoard`, so an extra engine added from the Agents
  menu sat on a stale port behind its tunnel with a green page. The WireGuard
  follower reaches any engine this node runs.

### Fixed
- **First-run setup refused anyone arriving over Tailscale**, telling them they
  were not on a private network. They were: `net.IP.IsPrivate()` covers RFC1918
  and RFC4193 and nothing else, while every tailnet node sits in 100.64.0.0/10,
  which is RFC 6598 shared address space. The refusal now covers that range too
  -- it is not routable from the internet, so the guard still does its job --
  and it names the address it saw, because a refusal you cannot argue with is
  the hard part.
- **And the way out it recommended did not work.** `hydra reset-password` wrote
  through a helper that updates an existing line and refuses when there is
  none, so on any config that had never carried an `[auth]` section -- every
  hand-written one, and every first run -- it failed with `key "password_hash"
  not found`. The section is created when absent, in the CLI and in the API
  path alike.

### Notes
- The tunnel's private key never enters the config tree, an `apply_config` push
  or an API response. The `.conf` stays in `<data_dir>/wireguard` at 0600 and is
  referenced by name; the file is stored and listed, never served back.
- Linux only. The Windows agent keeps naming an interface it manages itself.
- Needs `NET_ADMIN` (the container ships `iproute2` and `wireguard-tools`
  already). Missing it is reported at startup, naming the flag to add, rather
  than surfacing three layers away as an engine that announces nothing.
- A host that defaults IPv6 off on new interfaces -- Unraid does -- keeps the
  v4 half of a dual-stack provider config instead of losing the whole tunnel,
  and says which half it lost. Inside a container that half cannot be recovered
  from within: Docker mounts /proc/sys read-only whatever capabilities are
  granted, so enabling IPv6 on a freshly created device is not something the
  process can do. Measured, not assumed. The message names the fix
  (--sysctl net.ipv6.conf.default.disable_ipv6=0 on the container, or the same
  sysctl on the host under --network host) instead of leaving an operator
  looking for a missing kernel module.

## v3.155.1 - 2026-08-27

### Fixed
- **The new cross-engine probe reported a confident `unreachable` for a port it
  had not actually tested.** With no gluetun to publish the forwarded port, it
  fell back to the engine's own listen port -- which behind a NAT-PMP provider
  is the INTERNAL side of a mapping whose external port is a different number
  chosen by the provider. Dialling it from outside was always going to fail,
  whatever the state of the real forwarding, so the red dot said something the
  probe never measured.
  A failure is now only a verdict when the port dialled is one the provider told
  us about. Otherwise it stays `unknown` and says why, naming the port it tried
  and that no forwarded port is known for that engine.

## v3.155.0 - 2026-08-27

### Added
- **One engine now checks another engine's port forwarding.** A node whose
  engines leave by different tunnels can produce the one thing a single-homed
  node cannot buy: a peer arriving from somewhere else. The probe dials the
  target's forwarded port from another engine's exit and demands a BitTorrent
  handshake on a torrent the target serves, so only that engine's own peer_id
  can answer it.
  This replaces the apologetic `unknown` the self-probe had to return inside a
  tunnel, where the dial leaves and comes back to the provider's own address and
  the provider is under no obligation to return it. A failure now means the port
  really is shut, because the dial genuinely came from elsewhere.
  The prober is only used when its MEASURED exit address differs from the
  target's; two engines behind one tunnel learn nothing from each other, and the
  node falls back to the previous evidence chain.
- The cross probe dials the **forwarded** port when the provider reports a
  NAT-PMP mapping, which is the door peers actually knock on, rather than the
  engine's internal listen port.

### Fixed
- **The reachability check could confirm itself.** The cross probe opens a real
  peer connection to the target, the target counts it in `InboundAccepted`, and
  the passive branch reads that counter and reports "peers have connected to
  you" -- naming our own probe as the visitor. Arrivals this node caused are now
  discounted, so the passive evidence only counts peers we did not send
  ourselves. A green dot backed by nothing but itself is worse than the honest
  `unknown` it would have replaced.

## v3.154.0 - 2026-08-27

### Fixed
- **Typhon's own sockets are pinned to the interface too, so `bind_interface`
  now holds for peers as well as announces.** Go was sending the resolved IP as
  `outgoing_interfaces`, a key that does not exist in the engine's config: it
  was received and dropped, and the peer sockets were pinned by source address
  only, which steers nothing when every tunnel shares `10.2.0.2`. The engine
  now takes `bind_device` (the interface NAME) and applies `SO_BINDTODEVICE` to
  the peer listener, the outbound peer dials and the uTP UDP socket.
- **Inbound on a second tunnel could not answer.** The listener accepted a peer
  arriving on the non-default tunnel, because both interfaces carry the same
  address, and the reply then left by the default route with a different public
  address; the peer dropped it and the connection died with nothing logged.
  Pinning the listener makes every accepted socket inherit the device, so the
  reply goes back the way the peer came.
- A socket that cannot be pinned now fails: the listener refuses to bind and a
  dial is abandoned, rather than falling back to the default route. Failing is
  visible, and a silent fallback publishes the address the tunnel exists to hide.

### Not measured
- The outbound half rests on the same `SO_BINDTODEVICE` call proven on the bench
  for the announce path in 3.153.0. The inbound half (an accepted socket
  inheriting the device) is deduced from how Linux routes a reply, not observed:
  measuring it needs an external initiator hitting each tunnel's forwarded port.

## v3.153.0 - 2026-08-27

### Fixed
- **`bind_interface` pinned nothing on a multi-tunnel VPN, and every engine
  announced through one tunnel.** The pin resolved the interface to its IPv4 and
  set that as the dial's source address. ProtonVPN hands EVERY tunnel the same
  `10.2.0.2`, so the two engines asked for the same source, and a source address
  does not choose a route: the kernel sent both out whichever tunnel held the
  default route. No error, no log, and the header showed one exit address for
  every engine, which is how the report came in.
  Measured on two Proton servers (FR#173, FR#373) in a dedicated netns:
  source-IP bind gave `203.0.113.21` for wg0 AND wg1; `SO_BINDTODEVICE` gives
  `203.0.113.21` and `203.0.113.22`. The announce path, the exit-address
  probe and the reachability probe now pin by DEVICE, with the source address
  kept as well when the interface carries an IPv4 (Windows, which has no
  `SO_BINDTODEVICE`, still pins the source address only).
- **A `udp://` tracker announce ignored `bind_interface` entirely.** The
  announcer never carried the interface name to its UDP twin, so those announces
  left by the default route however the engine was configured, while the
  `http://` ones were pinned. They now take the same pin, and refuse to announce
  rather than fall back when the interface does not resolve.

### Known gap
- Typhon's own peer sockets are still pinned by source address
  (`bindings[].listen_addr`), and the `outgoing_interfaces` key Go sends it does
  not exist in the engine's config at all: it is received and dropped. So on a
  Proton-style setup the PEER traffic of two engines can still share one tunnel.
  Announces are what a tracker records, and they are fixed here; the peer side
  needs the same device pin on the Rust listener and is not done.

## v3.152.0 - 2026-08-27

### Changed
- **An import no longer re-downloads what the source said it already had.**
  A torrent qBittorrent or Transmission reported as complete is adopted with the
  same per-file check the Add form's "skip the hash check" uses: every declared
  file is stat-ed, sizes included, under the layout Typhon writes. When they are
  not there the torrent is refused, naming the path we looked at, and the import
  carries on with the next one.
  The qBit import used to fall back to a plain add when the content path was
  missing, which turned a path-mapping mistake into a re-fetch of the whole
  library; Transmission's complete branch trusted the resume file with no check
  at all and would announce us as a seeder holding nothing. Both are gone. A
  torrent the source reported as INCOMPLETE still downloads, as it should.

## v3.151.0 - 2026-08-27

### Added
- **The Add form now decides the on-disk shape and the hash check per add.**
  Two checkboxes under Advanced: *put the payload in its own subfolder*, which
  starts on the daemon's `create_torrent_folder` and overrides it for that add
  only, and *skip the hash check*, which adds in seed mode over data already on
  disk. Both travel through the whole add path: JSON `/api/torrents`, the
  multipart upload, the magnet resolution that re-enters it, and the routed add
  to a remote agent (`create_folder` / `skip_recheck` in `AddRoutedParams`, both
  omitempty, so an older controller keeps its old behaviour).
- `GET /api/torrents/add-defaults` reports the daemon's defaults so the form
  states what will happen instead of guessing. Until it answers, the form sends
  no override at all rather than an unchecked box that would silently turn
  `create_torrent_folder` off for that add.
- **Skipping the hash check now verifies the payload instead of trusting it.**
  Every file the torrent declares is stat-ed under the layout Typhon writes,
  `<engine save_path>/<info.name if multi-file>/<BEP-3 path>`, and the add is
  refused, naming the missing or wrong-sized files, when they are not there.
  Seed mode does not fall back to downloading: without this check a wrong save
  path produced a torrent stuck at 100% that could not serve a byte.
  The qBittorrent shim's own `skip_checking` path is untouched, cross-seed's
  contract with it predates these options.

## v3.150.2 - 2026-08-26

### Fixed
- The network panel was see-through. It took `--bg-secondary`, which is 75%
  opaque because the cards that use it sit on a quiet background; this one opens
  over the torrent table and lists addresses, and the rows underneath came
  straight through the text.

## v3.150.1 - 2026-08-26

### Fixed
- **The header stopped showing the IPv6 address.** The v6 measurement added in
  3.149.0 was gated on `enable_ipv6` read from `ComposeSession`, which zeroes
  that key and `listen_port` deliberately -- it composes what the front PUSHES
  to an agent, where both belong to the agent. The flag was therefore always
  false and no v6 address was ever measured, on a node whose config says true.
  The address was never lost; the display had stopped looking for it. Both this
  and the reachability probe now read the RESOLVED engine config, which is what
  the engine actually runs.

## v3.150.0 - 2026-08-26

### Added
- **Every engine has a reachability probe, not just the two primaries.** The
  probe named "race" and "hoard" in six places, so an engine added from the
  Agents page had none at all: its vertex in the header stayed amber saying so.
  Each engine is now probed under its own key, asked its own engine whether a
  peer has already arrived, and given its own deadline -- with one budget for
  the pass, the last engine of a large node inherited whatever the first left,
  and a probe that runs out of time is indistinguishable from a closed port.
- Each engine is probed **at its own exit address**, the one measured through
  its own binding, instead of the process's default route. On a node whose
  engines leave by different tunnels those are different addresses, and probing
  the wrong one answers a question about somebody else's port.

## v3.149.0 - 2026-08-26

### Added
- **The header shows one polygon, one vertex per engine.** Two labelled dots
  said "Race" and "Hoard" because a node was those two engines; an engine added
  from the Agents page had no dot at all. The shape grows instead: a point, a
  segment, a triangle, an octagon. The ring joining the vertices is deliberately
  neutral -- a coloured edge would suggest a link between engines that does not
  exist -- so only the vertices carry state. Clicking it opens a panel that
  grows out of the polygon, with each engine's interface, port and exit address.
- **Each engine's exit address is measured through its own binding**, the way
  the Network tab's check measures it. `getPublicIP()` asks an echo service from
  the process, so it reports the DEFAULT ROUTE: give three engines three tunnels
  and the header, and every Exit IP line on the Agents page, showed the same
  address for all of them -- and an engine leaking outside its tunnel looked
  exactly like one inside it.
- The header prints an address only when there is ONE. Several engines behind
  several tunnels have several exits; naming one of them would name an address
  most of the traffic does not leave by. It says how many instead, and the panel
  lists them.

### Note
- An extra engine has no reachability probe of its own yet, so its vertex stays
  amber with "no reachability probe for extra engines yet" rather than claiming
  a green nobody measured.

## v3.148.1 - 2026-08-26

### Fixed
- **A move on an agent could move a whole category.** When a payload sits loose
  in the category directory, its content root IS that directory, so relocating
  it takes every other torrent in the category along. This node's own engines
  have refused that for a while; the agent path did not, because only this side
  knows what a category directory is over there. Measured on staging, where a
  category change moved the entire `movies` folder into `series`.

## v3.148.0 - 2026-08-26

### Added
- **Moving to a category now moves the files, wherever the torrent lives.** On
  this node's primaries it always did; on an agent -- or on any other engine of
  this machine -- it relabelled the torrent and moved nothing, leaving the
  payload in the old category's directory for good. Planning from here compared
  that node's paths with this host's, so the honest answer at the time was to do
  nothing.
- The node holding the files does the work: it plans in its own filesystem,
  moves in the background (a copy across filesystems runs for hours) and reports
  how far it got. This side keeps the decision, the durable job and the
  progress; the torrent is stopped for the move and restarted whatever happens.
- Its refusals are the ones the local move already gives -- hardlinks, no room,
  target exists -- so the existing "this would break N hardlinks, do it anyway?"
  prompt covers an agent's torrents without a line of UI change.

## v3.147.5 - 2026-08-26

### Fixed
- The engine id inside the label call, the free-space probe and the front-only
  dialers now follow the same rule as the rest: each end of a move names its
  own. Missing one of them was how a delivered torrent still lost its category.

## v3.147.4 - 2026-08-26

### Fixed
- The engine id inside the label call, the free-space probe and the front-only
  dialers followed the same rule as the rest: each end names its own. Missing
  one of them was how a delivered torrent still lost its category, with the
  reason in a warning nobody reads.
- **A move named one engine for both ends.** That was true while a move meant
  "the same engine on another machine"; handing a torrent from `local-hoard` to
  `local-vpn7` asks for a different engine on each side, and the single field
  sent whichever was resolved first to both -- `agent "local-hoard" has no
  engine "movetest"`, after the job had been accepted. Each end names its own
  engine now, and a job written by an older version still reads.

## v3.147.3 - 2026-08-26

### Fixed
- **A torrent handed to another engine arrived without its category**, which
  then made it unmovable, since the destination path of a move comes from the
  category. The move named the target engine by ROLE; the routed calls on the
  far side resolve by engine id alone and refused it with "engine not wired",
  and labelling is deliberately never fatal, so the failure was a log line and a
  torrent that looked fine until the next move refused to start.

## v3.147.2 - 2026-08-26

### Fixed
- **A move was only durable five minutes later.** Each engine writes its torrent
  set to its database on a timer, and nothing flushed either end when a move
  finished: a restart inside that window brought the torrent back on the source,
  which still had a row saying it held it, and lost it on the target, which had
  none -- two engines pointing at one set of files, the exact outcome the
  handoff ordering exists to prevent. Both ends are now written before the job
  reports success. Found by restarting staging a minute after a handoff.

## v3.147.1 - 2026-08-26

### Fixed
- **A torrent on an extra engine was invisible everywhere.** The aggregate
  deliberately skips this node's own agents -- the local path already reports
  them, and including them counted every torrent twice in 3.135.0 -- but that
  is only true of the two primaries. Every other engine of this node was read by
  nobody: absent from both lists, uncounted in the totals, out of reach of a
  per-torrent action. Handing a torrent to such an engine looked exactly like
  losing it. The exclusion now names the two engines it is actually about.

## v3.147.0 - 2026-08-26

### Added
- **Send a torrent to another engine of this machine, from the right-click
  menu.** The Agent group was hidden whenever no REMOTE agent existed, which is
  every node that runs several engines and nothing else -- so the one thing
  extra engines are for, moving a torrent onto another tunnel, could not be
  asked for. Engines of this node are offered as destinations now.
- Between two engines of one machine the payload never moves: they share a
  filesystem and the files are already where the target expects them, so the
  torrent changes hands where it lies -- seconds, whatever its size. The source
  stops before the target adopts, so the same files are never open for writing
  twice, and the source is always released with its data kept.
- A handoff cannot duplicate, and the menu no longer offers it: two engines
  seeding one set of files are two writers on the same bytes the first time
  either repairs a piece.

### Fixed
- A move submitted without an engine assumed `hoard`. The engine now comes from
  the source agent, which names exactly one since one agent became one engine --
  a race torrent would otherwise have resolved to an engine that does not hold
  it.

## v3.146.0 - 2026-08-26

### Fixed
- **The Agent column said "local" for every torrent on this node.** The race
  list wrote that literal into each row and the hoard list wrote nothing at all,
  so the UI filled in the same word. It stopped being a name in 3.138.0: this
  node is `local-race`, `local-hoard` and one agent per extra engine. Rows now
  carry the agent that actually holds them.
- Every "is this row mine" test now accepts those names, on both sides. The
  daemon compared against the literal `"local"` in six places -- torrent detail,
  files, availability, pause -- and the browser in ten, including the live SSE
  path: a row whose agent read `local-hoard` would have had its stats updates
  skipped, its removal ignored, and a per-row action dialled at an agent nobody
  registered. The two rules are now the same rule, `isLocalAgentName` and
  `_isLocalAgent`, and a test pins them together.
- A detail request that names this node stops searching there instead of falling
  through to "which agent owns this hash". With the same torrent seeded here and
  on an agent -- the normal case after a cross-seed -- the page could answer with
  the other machine's figures under this node's name.

## v3.145.0 - 2026-08-26

### Changed
- **One table on the Agents page, because there is one thing.** It listed the
  agents, then listed "Engines on this machine" -- the same rows again, with
  different columns and a different verdict on what could be done to them: an
  agent could not be deleted, the engine behind it could. One agent has been one
  engine since 3.138.0, so the split described a distinction the daemon no
  longer makes. Each row now carries its engine's role, live port and interface,
  and its delete button removes the thing itself.

## v3.144.0 - 2026-08-26

### Added
- **Every engine is configurable, not just the first two.** The Network tab
  showed exactly two interface rows and two port fields -- race and hoard, in
  the code as much as on screen -- so an engine added from the Agents menu had
  nowhere to be configured. It ran on a copy of its role's primary and could
  never be given a tunnel of its own, which was the entire point of a per-engine
  interface. The page now has one row per engine this node runs.
- **An extra engine takes a pushed config like every other agent.** Its server
  was left without a config manager, so `apply_config` answered "this node
  configures itself locally" -- and nothing did. A settings change reached every
  other node in seconds and stopped there, silently, while the Agents page
  reported the engine online and current. Its announcer is rebuilt on the way
  through: the bindings are computed once, so an engine moved to another tunnel
  would otherwise have kept announcing through the old one.
- The `[[agent]]` fold now runs at boot. `MigrateSidecars` had done the
  rewriting since it was written and nothing ever called it, so `engines.json`
  stayed the live source and the array stayed a plan. It is additive and
  reversible: the previous config is kept as `.bak-migrate` and the sidecar is
  renamed rather than deleted.

### Changed
- An engine entry holds what is true of THAT engine -- its port, its interface
  -- and inherits everything else from the `[race]`/`[hoard]` profile for its
  role. The sidecar it replaces froze a copy of the primary's entire config at
  creation and went stale the moment anything changed, which is how an extra
  engine ended up announcing through last month's tunnel while every page
  reported green. The sync that copied the primary's egress onto every shard on
  each save is gone with the drift it was patching over.
- Saving the Network tab only asks for a restart when a listen port actually
  changes. A port is the one setting a running engine keeps across a config
  apply; everything else on that page reaches the engines within seconds, and a
  banner shown anyway taught people to restart -- dropping every peer connection
  -- for changes that were already live.
- `/api/engines` reports what is RUNNING rather than what a file says. The file
  listing showed an engine that had failed to start and hid one a restart had
  picked up from a hand-written entry; both read as "everything is fine".

### Fixed
- A locally-hosted `[[agent]]` entry ran with every field it did not mention set
  to zero -- no connection limit, no peer timeout -- because its session was
  decoded into a typed struct where "absent" and "written as zero" are the same
  thing. It is merged over the role profile now, the same way a remote agent's
  `[[agent.engine]]` override always was.
- A config push to such an engine composed the fleet profile without the entry's
  own keys, so the first apply would have moved the engine back onto the
  profile's interface -- announcing from an address nobody chose.

## v3.143.0 - 2026-08-26

### Added
- **Adding an engine starts it, no restart.** "+ New / this machine" on the
  Agents page now spawns the engine, opens its store, starts its announcer and
  registers it as its own agent (`local-<id>`) before answering the request --
  where it used to write `engines.json` and put up a restart banner. Deleting one
  stops it the same way, in the order that keeps the swarm honest: announcer
  first, then the engine, then a last store reconcile, then the process.
- The hot config apply that landed in 3.142.0 could only restart engines it had
  been handed at boot, so a brand new one had nothing to bring it into existence
  and was skipped in silence. The engines of this node are now owned by one
  manager for the life of the process, and a boot and a hot add go through the
  same spawn, the same registration and the same teardown.

### Fixed
- An engine that fails to start is no longer written to `engines.json`. It would
  have failed identically at every boot from then on, with the reason buried in
  the startup log instead of being the answer to the request that asked for it.
- Extra engines were registered with a copy of their process's client rather
  than the stable handle. A later settings change replaces the process, and
  every holder of such a copy keeps writing into a socket that closed with it --
  the failure mode 3.142.0 removed for the two primaries, still open here.

## v3.142.1 - 2026-08-25

### Fixed
- **The watchdog undid every hot config apply.** It held the engine process it
  was handed at boot, so when a settings change replaced that process the
  watchdog polled the retired pid thirty seconds later, called it dead and
  restarted the whole daemon -- exactly what applying the config without a
  restart was for. It now asks which process is current on each tick.
- This node's engines were left out of the config push entirely: it iterated the
  snapshot that deliberately hides them, the one that exists so the counters do
  not double. Pushing a config is not counting, so it uses the full list.

## v3.142.0 - 2026-08-25

### Added
- **This node applies a settings change without restarting Hydra.** Its engines
  took a pushed config like any other node's now: the ones whose settings
  actually changed are restarted, the rest are left alone. Remote agents have
  worked this way for a while; the monolith waited for a full restart, so a
  fleet ran two different configurations with nothing on screen saying so.
- A push never overwrites this node's `listen_port` or `enable_ipv6`. They are
  zeroed on the wire because on a remote node they belong to the agent; here the
  front is the agent, so applying them verbatim would have set the listen port
  to zero on the first reload and taken the node off the swarm.

## v3.141.0 - 2026-08-25

### Changed
- Internal: everything that talks to an engine now holds a stable handle
  (`EngineRef`) instead of a copy of its client. No behaviour change today; it
  is what makes restarting an engine without restarting Hydra possible at all.

  A client does not survive its process: `ltclient` dials its two sockets once
  and never redials, and `EngineProcess` holds exactly one client created with
  it. Twenty-two places took a copy -- including both tracker announcers, which
  would have gone on announcing into a closed socket while the engine ran fine
  beside them, with no error and no log.

## v3.140.1 - 2026-08-25

### Added
- Internal: `MigrateSidecars` folds `engines.json` into `[[agent]]` entries.
  **Deliberately not run automatically.** Rewriting someone's `default.toml` at
  boot is the riskiest thing this codebase could do, and the reader added in
  3.140.0 already accepts the new shape, so the array can be adopted by hand at
  no risk. The function is tested and callable; wiring it to a boot path is a
  decision on its own.

## v3.140.0 - 2026-08-25

### Added
- **An `[[agent]]` entry with no `addr` now describes an engine that runs here.**
  One array for every node: `addr` present means reached over the network,
  absent means started by this process. It is the shape the config converges on
  now that one agent means one engine, and it makes `[race]`/`[hoard]` what they
  already half were -- fleet-wide profiles per role.

  ```toml
  [[agent]]
  name = "local-vpn7"
  role = "race"
    [agent.session]
    listen_port = 26991
    bind_interface = "wg7"
  ```

  Additive, unlike `[[engine]]` blocks: the primaries are not displaced. Reusing
  a primary's id overrides that engine instead of colliding with it. A `role` is
  required, so an entry that merely forgot its `addr` is not silently started
  here.

Nothing writes these yet; existing configs are unaffected.

## v3.139.2 - 2026-08-25

### Changed
- Internal: the TOML editor can now edit and delete `[[array]]` blocks, selected
  by a key inside them. Nothing uses it yet. It is the missing brick for moving
  every node -- local or remote -- into a single `[[agent]]` array, which is
  where the config is heading now that one agent means one engine.

## v3.139.1 - 2026-08-25

### Fixed
- **The Network tab silently wrote into a section the daemon ignores.** A config
  using `[[engine]]` blocks never reads `[race]` or `[hoard]` -- the blocks
  replace them entirely -- but this page only ever wrote those two sections. On
  such a node every save reported success and changed nothing, before or after a
  restart. The page now says so, and refuses the save instead of pretending.

## v3.139.0 - 2026-08-25

### Changed
- **One form to add a node, wherever it runs.** "+ New" in Agents now asks
  whether it runs on this machine or another one. "This machine" starts an
  engine here and it becomes the agent `local-<id>`; "another machine"
  registers one already running elsewhere. There was previously no way to
  create a local agent at all: the Agents form demanded an address, and the
  separate "Add engine" form never said the word agent.
- The separate "Add engine" form is gone. The engines table stays, and now shows
  which agent carries each engine, since that is the name a category references.

## v3.138.1 - 2026-08-25

### Fixed
- **The Engines screen lost its built-in engines.** It looked for an agent
  literally named `local`, which stopped existing in 3.136.0 when this node
  became one agent per engine, so the table silently dropped the race and hoard
  rows and showed only the extra engines.
- The move menu would have offered this node's own engines as if they were other
  machines. A move to "another node" that is in fact this one is not a move;
  intra-node moves are a real feature but not that menu.

## v3.138.0 - 2026-08-25

### Changed
- **Every extra engine is now its own agent.** They were dialled back over a
  loopback port as a single agent called `local-shards` -- N engines behind one
  name, so a category could target "the shards" but never a particular one. An
  engine with id `vpn7` is now the agent `local-vpn7`, nameable in a placement
  list like any other. Naming one pins its engine, whatever the category's mode
  says.
- The loopback listener, its port and its generated token are gone: the front
  calls into these engines instead of dialling itself.

### Removed
- The `local-shards` agent. Any placement list naming it must be updated to the
  per-engine names.

## v3.137.2 - 2026-08-25

### Fixed
- **A per-agent save path set for `local` stopped being applied in 3.136.0.**
  Splitting this node into `local-race` and `local-hoard` made that key match no
  agent, so the override was silently ignored and the category fell back to its
  flat `save_path` -- torrents landing on the disk the operator had deliberately
  moved them off, with nothing logged. The legacy key is honoured again for both
  engines, and an exact per-engine key still wins over it.

## v3.137.1 - 2026-08-25

### Added
- `hydra_agent_row_deltas_total` in `/metrics`: rows updated from an agent's
  event stream rather than from a full re-listing. A stream that dies quietly,
  with the poll covering for it, looks exactly like a working one -- the only
  way to tell is that this stops climbing. It has to be visible before the
  polling cadence is relaxed on the strength of it.

## v3.137.0 - 2026-08-25

### Added
- **Remote agents' rows now follow their event stream.** Their torrents' rates
  and peer counts update from the delta the engine already emits every second
  instead of waiting for the next full re-listing, so the table is live between
  polls rather than only at them.

The polling cadence is unchanged on purpose. This release only makes the cache
fresher; relaxing the polling is a separate step, taken once a live deployment
shows the streams are really carrying the traffic. An agent whose stream cannot
be opened keeps being polled exactly as before.

## v3.136.2 - 2026-08-25

### Changed
- Internal: the row cache can now apply an agent's event stream instead of
  re-listing. Nothing subscribes yet, so no behaviour change. A stats delta
  updates rows in place and may never create one; adds ask for a refresh
  because they do not carry enough to build a row.

## v3.136.1 - 2026-08-25

### Changed
- Internal: the agent row cache is keyed per row instead of being a flat list.
  No behaviour change. It is the shape a single torrent's update needs, and the
  step before the cache stops being rebuilt from a full re-listing on every
  refresh -- which costs 209 ms and 271 MB at 198k torrents.

## v3.136.0 - 2026-08-25

### Added
- **One agent per engine on this node.** The race engine and the hoard engine
  are now two separate agents, `local-race` and `local-hoard`, so a category can
  send race torrents out of one tunnel and hoard torrents out of another on the
  same machine. Naming one of them in a placement list pins that engine even if
  the category's mode says otherwise.

### Changed
- The agents list shows two entries for this node instead of one.
- `local-race` and `local-hoard` join `local` as names a dialled agent may not
  take, so a remote node cannot claim one and have every action meant for it run
  here instead.

`local` keeps working and means exactly what it always did: this node, with the
engine chosen by the mode. It stays valid in categories, save-path overrides and
job params, so nothing needs migrating.

## v3.135.2 - 2026-08-25

### Fixed
- **The double counting was only half fixed in 3.135.1.** That release excluded
  this node from the hoard row collector, but three more places combine the
  local contribution with the agent list the same way, so the race engine still
  reported 28 torrents for 14. `agentsSnapshot` now excludes this node by
  default -- which is what all twelve of its callers were written to assume --
  and the two views that present agents by name ask for the full list
  explicitly.

**Do not run v3.135.0 or v3.135.1.**

## v3.135.1 - 2026-08-25

### Fixed
- **Every torrent was counted twice.** 3.135.0 made this node's engines a
  registered agent, which silently enrolled them in the agent-row collector --
  whose totals are added ON TOP of the local counters. `/api/status` reported
  396592 torrents for the 198296 the database holds, and every listing doubled
  with it. Nothing was written and nothing could be: `info_hash` is a primary
  key. Rolled back in production within minutes of the reading; anyone who ran
  3.135.0 should upgrade rather than trust any count it showed.

**Do not run v3.135.0.**

## v3.135.0 - 2026-08-25

### Changed
- **This node's own engines are now a registered agent instead of a special
  case.** They were previously synthesised into the agents list and hardcoded
  as the name "local" in the placement. One code path now addresses every
  engine, local or remote, which is what one agent per engine needs.
- **"Online" for a local engine now means it answered.** The synthesised entry
  reported an engine online from a non-nil pointer alone and never pinged it,
  so a wedged local engine showed green here while every action against it
  hung. It is pinged like any other now.

- The local node's exit IP no longer goes blank. `internal/agent` keeps its own
  public-IP cache, which is cold at that point and holds a retry backoff after
  its first failure, so the field a synthesised entry used to fill came back
  empty and stayed empty. A local node's egress is this daemon's egress, so it
  falls back to the value this package already tracks.

Nothing is renamed: the agent is still called `local` and still hosts both
engines, so existing categories, placements and save paths are untouched.

## v3.134.4 - 2026-08-25

### Changed
- Internal: `AddLocalAgent` registers an engine of this process under an agent
  name, with no dialling and no discovery round-trip. Still unused, so no
  behaviour change. Unlike the remote path it accumulates engines under one
  name instead of replacing them, because it is called once per engine.

## v3.134.3 - 2026-08-25

### Changed
- Internal: this process can now address its own agent server without a
  listener, a port or a token (`agent.InProcessStub` +
  `grpcclient.NewWithStub`). Nothing uses it yet, so no behaviour change. The
  local path runs the same handlers and the same encodings as the remote one,
  which is what stops a local engine from ever answering differently.

## v3.134.2 - 2026-08-25

### Changed
- Internal: an engine running in this process can now present itself as an
  agent (`localAgentClient`). Not yet wired to anything, so no behaviour
  change. Listing, stats and per-torrent reads go straight to the engine, while
  node-level calls reuse the agent server that already carries the shard
  traffic: performance where it was measured to matter, proven code elsewhere.

## v3.134.1 - 2026-08-25

### Changed
- Internal: the agent registry now holds an `AgentClient` interface instead of
  a concrete gRPC client. No behaviour change. This is the first step toward
  one engine per agent, which needs an engine running in this process to be
  registrable exactly like a remote one. The compiler checked the whole
  surface: 27 methods, 43 call sites, and four methods that were being used
  without anyone having listed them.

## v3.134.0 - 2026-08-25

### Fixed
- **Extra local engines kept the tunnel they were born with.** A shard created
  from the Agents menu stores its own copy of the egress settings in
  `engines.json`, frozen at creation. Every later save on the Network tab
  rewrote `[race]`/`[hoard]` and left the shards pointed at the old interface
  or the old proxy: nothing failed, the page showed the new setting, and the
  shard went on announcing from the old address. Saving the network settings
  now carries the same egress decision to every extra engine of that role. Its
  own listen port, id and role are left alone -- two engines sharing a port
  leaves the second dead at boot.
- **The network check could not see extra engines at all.** A shard on a stale
  or broken tunnel was invisible while every line above it reported green. Each
  one is now measured through its own announce client, and a shard whose
  interface disagrees with its role's engine is reported as a failure with both
  names side by side.

## v3.133.0 - 2026-08-25

### Changed
- **Renumbered to vacate 3.132.x.** Behaviour is identical to v3.132.1; this
  release exists only to clear a version collision. Three different binaries
  were calling themselves `3.132.0` on 2026-08-25: this branch, an unpushed
  automation-engine branch, and the image that happened to be running on
  staging, which was neither. Two of them were local, but a version number that
  identifies more than one binary is worth nothing precisely when it matters
  most -- reading it off a running instance to find out what is deployed.
  Everything above 3.132.x is unambiguous again.

## v3.132.1 - 2026-08-25

### Fixed
- The network check built its own copy of the page fields and filled three of
  them, so the new per-engine warnings read zero values and reported *the race
  engine is bound to no interface* about two engines both bound to tun1. Caught
  on staging. There is now one reader of the config for both the page and the
  check, so a field added later cannot reach only half of its callers.

## v3.132.0 - 2026-08-25

### Added
- **One network interface per engine.** The Direct mode of the Network tab now
  takes a `bind_interface` for the race engine and another for the hoard
  engine, instead of one value shared by both. Give them two WireGuard tunnels
  to spread the two engines across two exit addresses, give them the same one
  to keep them together, or leave both empty on a host with no VPN. The TOML
  always had one key per section; it was this page that flattened them.

### Fixed
- **Announces ignored `bind_interface` and left by the default route.** The
  engine bound its peer sockets to the configured interface, but the tracker
  announce is dialled from the Go side and had no such pin, so peers travelled
  inside the tunnel while the tracker recorded this host's own address. No
  error, no log, and the network check reported green. Announces are now bound
  to the same interface as the peers, and an interface name that does not
  resolve makes the announce **fail** rather than fall back to the default
  route: a failed announce is visible, a silent fallback is not.
- A binding pinned to an interface no longer builds an IPv6 announce client.
  The pinned source address is that interface's IPv4, so every v6 announce on
  it could only fail while the dual-family report counted it as a live path.
  A tracker explicitly pinned to IPv6 on such an engine is now refused out
  loud instead of dialled from somewhere else.
- The network check measured the peer path on the hoard engine alone and
  labelled it as though it spoke for both. With one tunnel per engine that
  reported the hoard's address as the race engine's. Both paths are now
  measured, compared, and reported per engine — and the check no longer skips
  the announce/peer comparison in Direct mode, which is exactly where a
  per-engine interface now lives.
- A setup with one engine bound and the other on the default route is called
  out. It is the healthiest-looking failure available: the page shows a real
  tunnel, for half the traffic.

### Changed
- **The `vpn` network mode is now called `gluetun`.** Nothing in it was ever
  about VPNs in general: what separated it from Direct was the presence of
  `bind_interface`, and what it uniquely does is read a forwarded port off a
  gluetun control server. Interface binding moved down into Direct, where a
  bare-metal WireGuard host can finally sit. The mode is now deduced from the
  gluetun keys instead. **No config is rewritten**: an existing install with
  `bind_interface` set and no gluetun keys keeps working exactly as before and
  simply displays as Direct.

## v3.131.1 - 2026-08-24

### Fixed
- **The free-space reserve computed a refusal nobody read.** The add path
  called the variant that drops the error, so a category whose every agent sat
  below its reserve went on placing torrents on the fullest disk instead of
  refusing. The native add now answers 507 and says which category and which
  reserve.

## v3.131.0 - 2026-08-24

### Added
- **Placement strategies that look at the disk, not at a torrent count.**
  A category could only fan out to every agent or pick the one with the fewest
  torrents, and a torrent count is a poor proxy for anything: ten thousand
  small torrents and ten remuxes are the same number and nowhere near the same
  disk. Three strategies join it, all measured at the CATEGORY'S OWN PATH on
  each agent (categories already carry a per-agent path, and two agents can
  point the same category at very different filesystems):
  `most_free_space` (most room), `least_load` (fewest bytes/s moving right
  now) and `fill_then_next` (fill each agent in written order until it hits
  its reserve, so a collection stays on one node instead of being striped
  across all of them).
- **A free-space reserve per category (`min_free_bytes`).** An agent with less
  than that left at the category path gets no new torrents whatever the
  strategy, `all` included, and an add whose every candidate is below the
  reserve is refused rather than quietly sent to the fullest disk.

### Fixed
- **`least_load` was documented but never implemented.** Selecting it fell
  through to the default, which is fan-out: instead of balancing load it
  multi-homed every torrent onto every agent, silently.

## v3.130.3 - 2026-08-24

### Fixed
- **A race torrent stopped by the user read back as `queued` one tick later.**
  Stopping it stamped the intent on the cached row, but the periodic refresh
  rebuilds that row from the engine snapshot, and the engine has never heard of
  a user pause -- so the flag was dropped and the row reverted to the
  scheduler's own word for a hold it may undo. Visible in the UI, in
  `/api/race/torrents` (`user_paused: false`), and to anything reading either.

## v3.130.2 - 2026-08-24

### Fixed
- **A paused torrent kept accumulating seed time.** The eligibility test read
  the state string alone, but the race engine reports a user-stopped torrent as
  `queued` -- the intent lives in `user_paused` -- and `queued` is a state that
  legitimately counts (a scheduler holding a seed is still seeding). Measured
  on staging: 308 seconds credited over a 307 second pause. The counter now
  reads the intent flag as well.

## v3.130.1 - 2026-08-24

### Fixed
- **`seeding_time` was missing from the native list endpoints.** The field was
  on the struct, but the list rows are built by a hand-written projection that
  did not carry it, so `/api/hoard/torrents` and `/api/race/torrents` answered
  without it -- the exact endpoint a retention rule would read.

## v3.130.0 - 2026-08-24

### Added
- **A real cumulative seed time.** `seeding_time` was `now - completed_time`:
  a torrent stopped for a month still reported a month of seeding, and so did
  one whose machine had been off. It is now a counter that only advances while
  the torrent is actually available to seed -- complete, and not stopped by
  the user. A torrent held by a scheduler or force-choked by the disk slot
  manager still counts, because a tracker sees no difference. The counter
  lives in the store, keyed by info_hash alone, so it survives restarts and
  follows a torrent handed between the race and hoard engines.
- **`seeding_time` in the torrent list.** It used to exist only on the detail
  endpoint, so evaluating it over a catalogue meant one RPC per torrent.

### Fixed
- **The qBittorrent shim reported `seeding_time: 0` for everything.** Every
  external retention script -- \*arr, cross-seed, anything reading the shim --
  concluded "never seeded" about the entire catalogue. It now reports the real
  counter.

### Note
- Existing torrents have no history, so the upgrade seeds them **once** from
  the old formula as a documented upper bound; starting the whole catalogue at
  zero would have meant no retention rule could fire for weeks. Everything
  accumulated after the upgrade is really observed.

## v3.129.0 - 2026-08-24

### Added
- **The announce cadence is now graphed in the Benchmark tab.** Every number
  on that page described bytes; how often we actually talk to trackers, the
  one thing a tracker sees and rate-limits on, was visible only as a rolled-up
  log line when `announce_rate_limit` happened to bite. The engine now counts
  announces at the single point they all pass through (http:// and udp://,
  hoard and race, primary and secondary), the bench tick differences those
  counters into announces/second, and a new chart plots the per-engine cadence
  with failures as a dashed subset of it. Sizing a rate limit stops being an
  arithmetic guess from the torrent count.

## v3.128.0 - 2026-08-23

### Fixed
- **A cross-filesystem category move copied the whole payload and only then
  refused it.** Nothing checked the destination before the copy started: the
  only test happened once every byte was written, so a move onto an occupied
  path spent its entire copy to reach a refusal that was knowable in the first
  second. The destination is now examined during preflight and the move is
  refused up front, with `reason: "target_exists"` on the 409 so the UI can say
  what is in the way.
- **An empty destination directory blocked the move for no reason.** Sonarr and
  Radarr create the save path when they grab a release, well before anything is
  downloaded into it, so the ordinary case is a destination that exists and is
  empty. That is no longer an obstacle: an empty directory is removed and the
  payload renamed into its place. Anything with content in it still stops the
  move, untouched.
- **The refusal blamed a race that had not happened.** It always read "target
  appeared during the copy", including when the destination had been sitting
  there since before the move was submitted. It now says which of the two it
  was.
- **The category shown on a row changed even when nothing had moved.** The list
  repainted every selected row at click time, including rows whose payload was
  only queued for a background move that can fail hours later. Only a completed
  relabel updates the row now; a row being moved keeps its category until the
  job is done.

## v3.117.0 - 2026-08-22

### Fixed
- **Opening a torrent that lives on an agent showed nothing.** The hoard list
  already merged rows from every agent, but clicking one asked the local engine
  for the detail and the Content tab, so in a split front + agent deployment
  both came back empty for anything the controller did not hold itself. Detail
  and file listing now go to the agent that owns the torrent, and the row
  carries which agent that is, so the panel matches the line that was clicked.
  Thanks to @the-sblah (#29).
- **The UDP tracker wire test could not run under the race detector.** Its fake
  tracker recorded what it had been told from the serving goroutine while the
  test read those fields back on its own, and a UDP datagram is not a
  synchronisation edge between the two, so `go test -race ./internal/engine/`
  reported a data race and failed. The recorded fields now live behind a mutex
  and are copied out in one go. Test-only: nothing that ships was affected.

## v3.116.0 - 2026-08-22

### Fixed
- **A front that started before its agents were reachable never picked them
  up.** The boot dial ran once; an agent that was still starting, or briefly
  unreachable, stayed unregistered until the whole front was restarted. A
  background loop now re-dials the configured agents every minute until each
  one registers and answers a ping, so the order the machines boot in stops
  mattering. Thanks to @the-sblah (#28).
- **Deleting an agent declared in the TOML no longer undoes itself.** The
  retry loop replayed every `[[agent]]` block unconditionally, and a delete
  made from the Agents menu can only tombstone entries that live in
  agents.json, so an agent declared in the config came back on the next tick.
  A delete now records itself for TOML agents too, and the retry loop skips
  everything in the removed store; restoring an agent still brings it back at
  once.

## v3.115.0 - 2026-08-22

### Added
- **A dedicated agent node now answers a health probe.** Agent-only runs no
  api.Server, so a container built around one had nothing to reply to a health
  check with and every agent came up as "unknown" to whatever orchestrated it.
  The node now serves `GET /health` and nothing else: other paths 404, writes
  405. Healthy means every engine answers a ping rather than merely that the
  process is alive, because an engine can die with the gRPC server still
  replying normally, and a check that only proves the process exists keeps a
  node in rotation that can seed nothing. Each ping is bounded so a wedged
  engine fails the probe instead of hanging it, and the body names the engine
  that failed. The listener defaults to `[daemon] api_host:api_port`, free on
  an agent for want of an api.Server and already published by whatever runs the
  container; `--health-addr` moves it and `--health-addr=off` drops it
  entirely. Thanks to @the-sblah (#27).

## v3.114.0 - 2026-08-22

### Changed
- **A controller node no longer shows an Exit IP in its navbar.** In front-only
  mode the machine running the web UI drives remote agents and holds no engine
  of its own, so the address it egresses from is not the one any torrent
  announces from. Displaying it invited exactly the wrong conclusion when
  checking whether the fleet was leaking. The stat is now gone from the header
  in that mode, and with it the two-minute poll that fetched it; per-node
  addresses stay where they mean something, in the agents tab. Thanks to
  @the-sblah (#26).

## v3.113.0 - 2026-08-22

### Added
- **The agents tab now shows each node's IPv6 exit address next to its IPv4
  one.** The header had been reporting both families for a while, but a remote
  node in the agents table still showed a lone v4 line, so a dual-stack agent
  was indistinguishable from a v4-only one at a glance. Both places now render
  through the same helper: one masked line per family, every address repeated
  in the tooltip so a column too narrow to fit an IPv6 literal never becomes
  the only copy. When IPv6 is enabled in the settings but the host has none,
  the cell says so instead of quietly showing the v4 line alone, which used to
  pass for a working dual stack. Thanks to @the-sblah (#25).

## v3.112.0 - 2026-08-21

### Fixed
- **The slim listing added to v3.111.0 was fetched far more often than it
  needed to be, and gave back only part of what it should have.** Two causes,
  both in how it was cached. Its snapshots lived in a slot of their own, so the
  scheduling loops stopped piggybacking on a listing another caller had already
  paid for. And its lifetime was shorter than the loops' own period, so a 10s
  loop missed its previous snapshot every single firing by construction. The
  frames got 3.9x smaller but there were more of them, which ate most of the
  win.

  A slim request now takes a fresh full snapshot when there is one -- a full row
  carries every field a slim caller reads -- and the slim slot outlives the loop
  period. A full listing is still never served slim rows: that direction would
  blank most of TorrentStatus silently instead of failing.

## v3.111.0 - 2026-08-21

### Changed
- **`list_torrents` can return just the fields the scheduling loops read.** The
  announce reconcile, verify batching and download-slot loops poll the listing
  every 10 to 30 seconds and look at eight fields out of thirty-two. They now
  ask for `slim: true`, and the engine skips what the rest would cost: the name,
  save path, current tracker and announce error strings, and the three mutex
  acquisitions per torrent needed to read them. At 196k torrents this decode was
  54% of everything the Go side allocated.

  Both projections derive state, progress and bytes-done from one shared
  function, so they cannot disagree about a torrent. Without the parameter the
  response is byte-identical to before, and a remote agent answers with the full
  listing rather than needing a newer wire protocol -- the slim field set is a
  subset, so callers cannot tell.

## v3.110.0 - 2026-08-21

### Changed
- **`GET /api/port-forward` no longer builds the whole torrent listing to add
  up one integer.** It summed `num_peers` by materialising a 30-key map per
  torrent -- 196k of them per call -- for a total the engine already keeps in
  its cached stats and exposes through `GetAllStatus`. This was 2.2GB per 300s,
  17% of everything the process allocated. The endpoint is polled by the UI and
  sits in the logger's `SkipPaths`, so none of it appeared in the access log:
  it took an allocation profile to find, not a slow request.
- **The category filter moved into the hoard engine.** The adapter still had to
  copy all 196k `TorrentStats` before dropping the ones it did not want; the
  engine now filters while walking its own cache, so neither the copy nor the
  discarded rows happen.

## v3.109.0 - 2026-08-21

### Changed
- **Answering one category of the qBittorrent shim no longer builds the whole
  catalogue.** Sonarr and Radarr poll `/api/v2/torrents/info?category=...` once
  per category; the handler built a row for all 196k torrents and then threw
  away everything outside the category asked for. The five categories they poll
  hold 4.4k torrents between them, so 97.7% of that work was discarded -- and it
  was the largest single allocator in the process (2.4GB per 90s, 28% of all
  allocation). The hoard side now filters on the struct before building any row,
  and snapshots are cached per scope so a category poll neither builds nor
  invalidates the full listing. An unfiltered listing is unchanged.
- **The list decode no longer grows its slice from nothing.** `encoding/json`
  grows a slice it decodes into by repeated doubling, so decoding 196k torrents
  discarded roughly the whole array again in intermediates -- 2.2GB per 90s,
  25% of all allocation. The decode is now handed a slice sized from the
  previous one.
- **The shared list snapshot is no longer copied on every cache hit.** It is
  handed out directly and is read-only by contract: every caller ranges and
  reads, a refresh replaces the whole result rather than writing through it, and
  nothing sorts or mutates it. The copy it replaces guarded against a sort in
  `enforceDownloadSlots` that now runs only on slices that function derives
  itself.
- **`enforceDownloadSlots` stopped snapshotting the swarm-seed map.** It copied
  all 196k entries every 30s to look up the few thousand incomplete torrents;
  it now looks them up as it needs them, and sizes its index map for what it
  actually holds.

## v3.108.0 - 2026-08-21

### Fixed
- **Announcing leaked memory for as long as the process ran: ~0.3 GB/h at 196k
  torrents, 9 GB of live heap after 29 h.** None of the HTTP clients Hydra
  builds by hand set `TLSHandshakeTimeout`, and a `http.Transport` literal
  zero-values it, which means "no limit" -- only `http.DefaultTransport` sets
  one, and nothing here inherits from it. A tracker that accepted the TCP
  connection and then never finished the TLS handshake therefore blocked its
  dial forever. That alone would only cost one announce, but `net/http` purges
  `Transport.dialsInProgress` from the front only, so the stuck dial pinned
  every `wantConn` queued behind it, each retaining the `connectMethod` built
  from its announce URL. With keep-alives disabled every announce dials, so the
  queue grew by one retained object per announce: 36M live objects behind three
  pinned dials, for 54 requests actually in flight. The `http.Client` timeout
  never covered this -- the dial is detached from the request, so cancelling
  the request does not unblock it. All five hand-built transports now bound
  both the handshake and the response headers, and a test fails the build on
  any `http.Transport` literal that does not.
- **A SOCKS5 proxy that went silent mid-handshake blocked the dial forever.**
  Only the initial connect honoured the context; the greeting, the auth
  exchange and the CONNECT reply were deadline-free socket I/O. The handshake
  is now bounded (and never past the caller's own deadline), and the deadline
  is cleared before the connection is handed back so it cannot leak into the
  data phase.

## v3.107.1 - 2026-08-21

### Fixed
- **Adding a torrent to a remote agent no longer fails with `engine "race" not
  wired on agent`.** An agent names its engines from its own config, where the
  id is free-form (`race-0`) and the role is what says what the engine is. The
  add path put the *role* on the wire as the engine id, so the agent -- which
  indexes its engines by id -- found nothing unless the id happened to be
  spelled exactly like the role. Every other routed call already carried the
  real id, which is why only add broke, and only on agents whose ids are not
  literally `race`/`hoard`. The control plane now resolves a selector (id or
  role) to the agent's real engine id before the call, and an agent that hosts
  no such engine is reported as that, rather than as an opaque gRPC failure.

## v3.107.0 - 2026-08-21

### Fixed
- **The header no longer clips the IPv6 exit address.** The address line was
  capped at 23 characters, which is shorter than a full IPv6 literal, so a
  perfectly ordinary address was shown truncated with an ellipsis and had to be
  hovered to be read. The line is now sized for the longest address there is,
  39 characters, which the header has the room for.

## v3.106.0 - 2026-08-21

### Fixed
- **Torrents living on an agent are back in the hoard list.** The list is
  hydrated and kept live entirely over SSE, and that stream carried the local
  engine only, so a torrent placed on an agent had no row in /#hoard at all --
  it ran, announced and answered over /api/hoard/torrents while looking like it
  had vanished the moment it was moved. The agents' rows are now polled on one
  loop and published into the same stream, feeding the hydration, the live
  push, /api/hoard/torrents and the tab header from a single cache.
- **Stop and start work on an agent's torrents.** Both were applied to the
  local engine, which has never heard of the hash: a 404 on a monolith, and a
  silent no-op on a front-only node whose local engine answers "fine" to
  everything. The intent is now recorded through the agent's own engine, so its
  slot manager stops handing the torrent a slot on the next pass.
- **A routed add keeps its .torrent.** The blob was materialised into a file
  deleted on return, so the five-minute store reconcile -- which captures it by
  reading that path -- counted every routed add a miss and never inserted its
  row. The torrent then carried no category, save path or tags across a
  restart.
- **A front-only node has a working web UI.** Its empty hoard engine returned a
  nil event hub, and /api/events refuses to serve without one, which left the
  whole interface permanently empty.

## v3.105.0 - 2026-08-21

### Added
- **Move or duplicate a torrent between nodes, payload included.** Right-clicking
  a torrent offers an Agent group, present only when there are agents to send to,
  with "Duplicate to agent" (both nodes keep it) and "Move to agent" (the source
  is released once the target is verified and running). The destination is the
  path the torrent's own category defines for that agent, so re-categorising
  stays a separate action instead of being smuggled into a move.
- The bytes travel over the existing authenticated agent connection, one whole
  piece at a time, and every piece is checked against the SHA-1 already in the
  torrent before the receiver writes it. An interrupted transfer therefore
  resumes from the target's own bitfield: nothing else is recorded, because
  nothing else has to be. A move refuses up front when the target has no room,
  and either end may be the node running the front.
- **`POST /api/agents/:name/action`** runs one per-torrent action on a named
  agent: pause, resume, verify, reannounce, remove, and the two category forms.
  The per-torrent endpoints resolve their target by looking locally first, which
  is only unambiguous while a hash lives in one place.
- **`GET /api/agents/torrents`** returns the agent slice of the list on its own,
  so rows living on an agent stay current without reviving the full-list fetch
  that SSE hydration replaced.

### Fixed
- **`--agent-only` could never start on Windows**: the agent built a Unix socket
  path for its engine while the Windows engine only listens on TCP loopback, and
  the monolith's "one race and one hoard" check ran before the agent-only branch,
  so a dedicated agent had to declare engines it never runs.
- **A torrent added to an agent disappeared on restart**: the shipped `.torrent`
  was written to a temporary file deleted as soon as the add returned, while the
  engine kept that path as the torrent's durable location. Nothing was logged.
- **Torrents living on an agent were missing from the list entirely.** The REST
  list has aggregated agents all along, but the list stopped reading it when
  hydration moved to SSE, and SSE streamed only this node's engines.
- **An interrupted cross-node job died on the very restart it was built to
  survive**: unfinished jobs were resumed before the agents were registered.
- **Duplicates broke every "one hash, one place" assumption**: the list keyed
  rows by hash so the second copy replaced the first; selecting one row selected
  both; and per-torrent actions tried the local engines first, so they always hit
  the local copy. Rows and selections are now identified by node and hash.
- Changing the category of a torrent held by an agent no longer refuses with a
  cross-filesystem error measured between two different machines' paths, and
  editing its trackers no longer reports "no such torrent".
- **Deleting a torrent's data left its folder behind, empty.** The torrent's own
  folder is now pruned when emptied, and only ever its own: a single-file torrent
  stored unwrapped has the category directory as its save path.
- Every browser alert and confirm is now Hydra's own modal: styled, non-blocking,
  dismissible, and a confirmation can be declined.
- Context submenus open beside the menu, level with the row that opened them,
  instead of replacing the menu's contents.

## v3.100.0 - 2026-08-21

### Added
- **`HYDRA_LOG_STDOUT` streams the log to stdout instead of `hydra.log`.** Under
  Docker, systemd or any other supervisor the log belongs on stdout, where
  `docker logs` and the journal pick it up and rotate it; a file inside the
  config volume is the wrong place to look. Set the variable to anything but
  `0`/`false`/`no`/`off` and no `hydra.log` is opened at all. The mirror is
  attached when the logger is built rather than after the config path is
  resolved, so unlike the file mirror it also carries the lines logged while
  the config is still being read. `ERROR` is no longer echoed to stderr in that
  mode, since the mirror already puts it on the console. The startup banner
  names whichever destination is in use.
- The shipped `docker-compose.yml` now caps the Docker json-file log at
  128 MiB x 5, the ceiling the file mirror already applies to itself. The
  driver caps nothing by default, and this is a chatty log: it reached 41 GB in
  production before the file mirror was capped, and routing it to stdout
  without this would have brought that back.

### Fixed
- **The stdout mirror can no longer stall Hydra.** Every hub entry is formatted
  into the mirror while the hub lock is held, so a mirror that blocks blocks
  every log producer in the process -- the engines' stdout ingestion, gin,
  every `slog` call. A file write returns; a pipe whose reader has stopped does
  not. The stdout mirror therefore writes through a bounded queue and drops
  lines rather than blocking, reporting the number dropped as soon as the
  consumer catches up. The file mirror is unchanged.

## v3.99.0 - 2026-08-20

### Changed
- **Hydra now refuses to start when it is given a positional argument**, instead
  of ignoring it. `flag.Parse` stops at the first non-flag argument and drops
  every flag behind it in silence, so a compose `command:` that repeated the
  binary name -- the form the wiki used to document -- lost `--agent-only` /
  `--front-only` and booted two monoliths: the agent never opened its gRPC
  data-plane, the front came up with local engines of its own, and neither log
  said a word. If your `command:` starts with `hydra`, drop it: the container
  entrypoint already runs `hydra --config <config> "$@"`, so `command:` carries
  the extra flags only, e.g. `command: ["--agent-only", "--agent-addr", ":9090"]`.
  The error names the stray argument and, when it is the binary itself, prints
  that form. Subcommands (`hash-password`, `reset-password`, `set-listen-port`)
  are unaffected.

### Fixed
- A front-only node reports itself ready once its API is up. It has no local
  engine and no state to page back in, so it was fully started already, but the
  readiness flag was never set: `/api/startup` stayed `{"ready":false}` forever
  and the web UI sat behind its "Initializing..." overlay against a backend that
  was answering every other route. Not gated on the agents being reachable --
  agent health belongs to `/api/agents`, and gating here would restore the same
  permanent overlay the moment an agent went down.
- An `[[agent]]` block missing `name` or `addr` is logged instead of skipped in
  silence, naming the field at fault. The documented front-only config omits
  `name`, so the front dialed nothing and said nothing about it, which reads
  exactly like having no agents configured at all.

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
