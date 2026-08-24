package engine

// Cumulative seed time.
//
// Typhon's `seeding_time` is `now - completed_time`: a torrent that was
// stopped for a month still reports a month of seeding. That is the number
// retention rules and H&R checks would be built on, so it has to be a real
// accumulator before anything is built on top of it.
//
// What counts is AVAILABILITY, not transfer: a seed with no leecher, a
// choked seed, a torrent held by the disk slot manager's serving-suspend
// (which advertises "seedtime preserved") are all seeding as far as a tracker
// is concerned. So the clock runs for a torrent that is complete and not
// stopped by the user; only an explicit user stop halts it.
//
// The clock is package-level and keyed by info_hash alone, not per engine: a
// torrent handed off between race and hoard keeps its info_hash, so its
// history follows it with no extra code. It also means a hash somehow present
// in both engines accrues once, since the first accrual of a tick moves its
// timestamp forward.

import (
	"sync"
	"time"
)

// seedAccrualCap bounds a single accrual step. A gap longer than this means we
// were not watching (a stalled engine push, a suspended host), and crediting
// the whole gap would be inventing seed time we never observed.
const seedAccrualCap = 15 * time.Minute

// seedEntryTTL drops torrents nobody has reported for a while. The durable
// copy lives in the store, so a forgotten entry costs nothing but the memory
// it frees -- and at a million torrents that memory is the point.
const seedEntryTTL = 24 * time.Hour

// seedPruneInterval is how often the TTL sweep runs; a full scan of the map is
// milliseconds, but there is no reason to pay it on every stats tick.
const seedPruneInterval = time.Hour

type seedEntry struct {
	secs int64 // accumulated seconds of availability
	// seen is the last observation at which this torrent was ELIGIBLE, and 0
	// when it was not: that is what makes a stop cut the clock instead of
	// back-filling the whole pause on the next eligible tick.
	seen int64
	// touched is the last observation of any kind, eligible or not. Only the
	// TTL sweep reads it -- ageing entries out on `seen` would never expire a
	// torrent that sits stopped forever.
	touched int64
}

type seedClock struct {
	mu        sync.Mutex
	entries   map[string]seedEntry
	lastPrune time.Time
}

var seedClockSingleton = &seedClock{entries: map[string]seedEntry{}}

// SeedTimeSeed loads the durable counters at boot. Existing in-memory values
// win: the process has been observing since it started, the store copy is at
// best as fresh as the last sync tick.
func SeedTimeSeed(saved map[string]int64) {
	if len(saved) == 0 {
		return
	}
	c := seedClockSingleton
	c.mu.Lock()
	defer c.mu.Unlock()
	for ih, secs := range saved {
		if secs <= 0 {
			continue
		}
		e := c.entries[ih]
		if secs > e.secs {
			e.secs = secs
		}
		// Stamp touched so a seeded torrent the engines never report (removed
		// while we were down) ages out of memory like any other.
		if e.touched == 0 {
			e.touched = time.Now().Unix()
		}
		c.entries[ih] = e
	}
}

// SeedTimeFor returns the accumulated seed time of one torrent, in seconds.
func SeedTimeFor(infoHash string) int64 {
	c := seedClockSingleton
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.entries[infoHash].secs
}

// SeedTimeAll snapshots every counter, for the store sync tick.
func SeedTimeAll() map[string]int64 {
	c := seedClockSingleton
	c.mu.Lock()
	defer c.mu.Unlock()
	out := make(map[string]int64, len(c.entries))
	for ih, e := range c.entries {
		if e.secs > 0 {
			out[ih] = e.secs
		}
	}
	return out
}

// seedTimeEligible reports whether a torrent is available to seed right now:
// complete, and not stopped by the user. StateQueued (a scheduler holds it)
// and a serving-suspended torrent both still count -- see the file header.
func seedTimeEligible(s *TorrentStats) bool {
	return s != nil && s.Progress >= 1.0 && s.State != StateStopped
}

// accrueSeedTime advances the clock over a freshly built stats map and stamps
// the running total into each entry, so every consumer of TorrentStats (list,
// SSE, detail, the qBit shim) reads the real counter without a second lookup.
//
// Called before the map is published, so nothing else can observe a torrent
// whose SeedingTime is still zero.
func accrueSeedTime(newStats map[string]*TorrentStats) {
	if len(newStats) == 0 {
		return
	}
	now := time.Now()
	nowUnix := now.Unix()
	c := seedClockSingleton
	c.mu.Lock()
	defer c.mu.Unlock()
	for ih, s := range newStats {
		if s == nil {
			continue
		}
		e, known := c.entries[ih]
		if seedTimeEligible(s) {
			// A torrent seen for the first time only starts its clock now:
			// crediting anything here would be crediting time the process
			// was not running.
			if known && e.seen > 0 {
				delta := nowUnix - e.seen
				if delta > int64(seedAccrualCap/time.Second) {
					delta = int64(seedAccrualCap / time.Second)
				}
				if delta > 0 {
					e.secs += delta
				}
			}
			e.seen = nowUnix
		} else {
			// Not eligible: the clock stops, and the next eligible tick
			// restarts from that moment rather than back-filling the pause.
			e.seen = 0
		}
		e.touched = nowUnix
		c.entries[ih] = e
		s.SeedingTime = e.secs
	}
	if now.Sub(c.lastPrune) >= seedPruneInterval {
		c.lastPrune = now
		for ih, e := range c.entries {
			if e.touched > 0 && nowUnix-e.touched > int64(seedEntryTTL/time.Second) {
				delete(c.entries, ih)
			}
		}
	}
}
