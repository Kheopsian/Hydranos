package engine

// Per-engine announce counters.
//
// The announce cadence is the one number the tracker sees and we never did:
// the scheduler's own view is a plan, not what left the process. Counting at
// the single choke point every announce passes through (trackerAnnouncer.
// announce) gives the bench tick a cumulative figure it can difference into
// announces/second, for http:// and udp:// alike, hoard and race alike.
//
// The counters have to live in the package, not on the announcer: the race
// path builds one announcer per torrent, so a per-instance field would count
// one announce per counter and always read 1.

import (
	"sync"
	"sync/atomic"
)

// announceCounter holds the lifetime totals for one engine scope.
type announceCounter struct {
	// sent counts announces that actually went on the wire (or tried to).
	sent atomic.Uint64
	// failed counts the subset of sent that came back with an error.
	failed atomic.Uint64
	// limited counts announces dropped by announce_rate_limit before leaving,
	// i.e. the backlog the configured rate could not absorb.
	limited atomic.Uint64
}

var (
	announceStatsMu sync.RWMutex
	announceStats   = map[string]*announceCounter{}
)

// announceCounterFor returns the counter for a scope, creating it on first use.
func announceCounterFor(scope string) *announceCounter {
	if scope == "" {
		scope = "unknown"
	}
	announceStatsMu.RLock()
	c := announceStats[scope]
	announceStatsMu.RUnlock()
	if c != nil {
		return c
	}
	announceStatsMu.Lock()
	defer announceStatsMu.Unlock()
	if c = announceStats[scope]; c == nil {
		c = &announceCounter{}
		announceStats[scope] = c
	}
	return c
}

// AnnounceStats reports the lifetime announce totals for one engine scope
// ("race" / "hoard"). Monotone: the caller differences two reads to get a rate.
// An unknown scope reads zero rather than being created.
func AnnounceStats(scope string) (sent, failed, limited uint64) {
	announceStatsMu.RLock()
	c := announceStats[scope]
	announceStatsMu.RUnlock()
	if c == nil {
		return 0, 0, 0
	}
	return c.sent.Load(), c.failed.Load(), c.limited.Load()
}
