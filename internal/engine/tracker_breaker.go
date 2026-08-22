package engine

import (
	"log/slog"
	"sync"
	"time"
)

// A torrent is now announced to every tracker it carries rather than to one,
// so a tracker that is down no longer costs one failed announce per pass: it
// costs one per pass per torrent that lists it. Each of those is a dial that
// waits out its timeout, and the race keepalive walks a fixed budget of
// torrents per tick, so a single dead host can eat the whole budget and
// starve the trackers that are actually up.
//
// trackerBreaker keeps consecutive failures per host and stops announcing to
// that host for a cooldown once it has clearly stopped answering. Per host
// rather than per tracker URL: the passkey differs between torrents on the
// same tracker, the outage does not.
const (
	breakerFailThreshold = 5
	breakerCooldown      = 10 * time.Minute
)

type breakerState struct {
	fails int
	until time.Time
}

type trackerBreaker struct {
	mu    sync.Mutex
	hosts map[string]*breakerState
}

func newTrackerBreaker() *trackerBreaker {
	return &trackerBreaker{hosts: make(map[string]*breakerState)}
}

// allows reports whether an announce to this host may go out now.
func (b *trackerBreaker) allows(host string, now time.Time) bool {
	if b == nil || host == "" {
		return true
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	st := b.hosts[host]
	if st == nil {
		return true
	}
	return !now.Before(st.until)
}

// record folds one announce outcome in. A success clears the host outright:
// the breaker is there to spare a dead tracker, not to remember an old
// hiccup.
func (b *trackerBreaker) record(host string, ok bool, now time.Time) {
	if b == nil || host == "" {
		return
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	if ok {
		if _, tripped := b.hosts[host]; tripped {
			delete(b.hosts, host)
		}
		return
	}
	st := b.hosts[host]
	if st == nil {
		st = &breakerState{}
		b.hosts[host] = st
	}
	st.fails++
	if st.fails >= breakerFailThreshold && now.After(st.until) {
		st.until = now.Add(breakerCooldown)
		st.fails = 0
		slog.Warn("tracker breaker: host stopped answering, pausing announces",
			"host", host, "fails", breakerFailThreshold, "cooldown", breakerCooldown)
	}
}

// trippedHosts returns the hosts currently in cooldown, for observability.
func (b *trackerBreaker) trippedHosts(now time.Time) []string {
	if b == nil {
		return nil
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	var out []string
	for h, st := range b.hosts {
		if now.Before(st.until) {
			out = append(out, h)
		}
	}
	return out
}
