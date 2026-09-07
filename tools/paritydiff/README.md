# paritydiff -- the oracle for the Go to Rust port

Hydra 4.0.0 replaces the Go front with Rust, so that one process holds the
torrent state instead of two. The question that decides whether a slice of that
port is done is always the same: **does the new binary answer the same thing as
the old one?**

Porting the 19k lines of Go tests by hand would have ported their blind spots
along with their coverage, and still would not have answered that question. So
the port is checked differentially instead: both binaries are driven with the
same requests, against the same frozen store, and every response is compared
field by field.

## The pieces

| file | what it does |
| --- | --- |
| `routes.py` | Extracts the route table from the Go source. gin cannot be asked at runtime, and a hand-written checklist drifts. Generates `requests.json`. |
| `paritydiff.py` | Drives A and B with the same requests, compares status, `Content-Type` and JSON body. Exit code 1 on any difference, so it can gate a build. |
| `run-parity.sh` | The loop: build Rust, run its unit tests, stand it up, compare, tear down. |
| `routes.json` | The extracted table. 169 routes: 136 native, 32 qBit shim, 1 UI. |
| `store-schema.sql` | The frozen store schema, dumped from the live production database. |

## Running it

```bash
# on Orion
/mnt/cache/appdata/hydra-v4/tools/paritydiff/run-parity.sh --rebuild
```

The two daemons sit on the `v4bench` docker network, created `--internal`: it
has no route off the host. This is not tidiness. The bench replays production
data, and an instance that could reach a tracker would announce 243k torrents a
second time from the house IP. The engines are additionally started paused with
DHT, PEX and webseed off, so two independent things have to fail before anything
leaves the machine.

## What "identical" means

Some fields differ between two runs for reasons that have nothing to do with the
port -- a speed sampled a second apart, the container's own IP address. Those are
silenced, but only through declared entries:

* `NORMALISE` in `paritydiff.py` -- global, keyed on field name, each with its
  reason. Run `paritydiff.py --explain` to print the list.
* `IGNORES` in `routes.py` -- per route, for local truths like "the container's
  address belongs to the instance, not to the answer".

Every entry in `IGNORES` was **earned**: it appeared as a difference when two
identical Go instances were compared against each other. Anything that does not
show up in that twin run has no business being added. The rule matters, because
the failure mode of a tool like this is not being wrong, it is being trusted
while silently comparing nothing.

## The reference score

Two independent Go instances, same config, separate databases:

```
56/56 identical, 0 differing, 0 not implemented yet
```

That is the bar. A Rust slice is done when it does not lower it.

## Two traps already paid for

**`/api/events` is SSE.** The stream never closes, and a socket timeout never
fires on it because data keeps arriving, so reading it like an ordinary body
hangs the whole run. Streams are detected by `Content-Type` and sampled with a
deadline, then compared on **shape** -- field names -- rather than content, since
two instances legitimately emit their own events.

**`fnmatch` reads `[*]` as a character class.** Ignore patterns like
`$.entries[*].msg` matched nothing at all while looking perfectly applied. Paths
here are full of `[*]`, so the matcher translates globs itself and keeps brackets
literal.
