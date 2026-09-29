# Four clients at 290,000 torrents

At 50,000 torrents every client we tried works (see
[`benchmarks.md`](benchmarks.md)). This page is about roughly six times that,
which is far outside what most clients are designed for. None of what follows
is a bug report: each client makes reasonable choices for the libraries its
users actually have, and those choices only start to show at this size.

## How it was measured

Each client stood in for production on the same machine, with the same
library and the same six real trackers, one after the other between 21 and 24
September 2026. The Hydranos column is production over the same days, race
engine included. These are single, sequential runs against live trackers:
read them as orders of magnitude, not decimals.

The figure that matters most at this size is the announce rate. A
293,000-torrent catalogue on a 30-minute interval needs about **163 announces
per second** for every torrent to stay visible to its tracker.

| | Hydranos 4.2 | Transmission 4.1.3 | rtorrent 0.16.24 | qBittorrent 5.2.3 |
|---|---|---|---|---|
| Torrents | ~293,000 + race | 286,359 seeding | 286,157 | 286,059 |
| Cold start to API | 95–125 s | 48 min | 15 h 21 min | 91 min |
| RSS after load | 2.8 GiB (~10 KiB/torrent) | 12.3 GiB (~44 KiB) | 31.3 GiB (~114 KiB) | 15.6–18.3 GiB (57–64 KiB) |
| CPU after load | 3.4–3.7 cores (hoard + race) | 0.8–1.0 core | 1.2–1.3 cores | 0.7 core |
| Announces/s | 146–155 | 13.6 default, 61 with a raised limit | ~4–12 | ~15 default, 86 with a raised limit |
| Served | ~5 TB/day | 685 GB in 24.8 h (raised limit) | 0.15 GB in 23.7 h | < 0.05 MB/s over ~6 h |

Hydranos uses more CPU than the others here: that figure includes the race
engine and about 5 TB a day of upload, far more traffic than any of the other
runs carried.

## What shapes each result

- **Transmission** loads the library in 48 minutes. Its announces are paced by a
  compiled-in limit of 20 per 500 ms, a sensible default that keeps a client
  from flooding trackers. In practice it ran at about a third of that pace,
  and raising the limit fivefold took it from 13.6 to 61 announces a second;
  what holds it there was neither CPU nor a setting we found.
- **rtorrent** keeps its torrents in a list, which is simple and fast for the
  libraries it is usually given; at this size loading grows roughly with the
  square of the catalogue (28 minutes at 50,000, 15 hours at 286,000). It
  also opens at most 3 connections per tracker host, a deliberate courtesy to
  trackers that bounds its announce rate.
- **qBittorrent** (libtorrent) finds torrents in constant time and loads in
  91 minutes. Its announce rate is bound by tracker latency rather than by its
  concurrency setting; raising that setting to 512 brought it to 86 a second.

Hydranos makes the opposite trade-off on purpose: it is built for this size
first. For a few hundred torrents, qBittorrent or Transmission will serve you
with less to learn.
