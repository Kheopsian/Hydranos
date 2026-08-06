package api

import (
	"runtime/debug"
	"sync"
	"sync/atomic"
	"time"
)

// ---------------------------------------------------------------------------
// Hot-swappable optimisation flags for the HTTP/SSE side.
// ---------------------------------------------------------------------------
//
// Companion to internal/engine/ltclient/opt.go. Same rationale: each flag gates
// ONE optimisation so an A/B ladder can measure it without a restart, and a
// restart is expensive here — it resets the per-torrent upload counters, and
// trackers credit upload by MAX per torrent.
//
// Measured on prod 2026-08-06, 120s CPU profile at 107k torrents, as a share of
// the Go process:
//
//	qbitTorrentsInfo      10.8%  (the qBittorrent shim, NOT the web UI: the UI
//	                              list has been SSE-pushed since 3.2x)
//	startSnapshotPusher    7.7%  of which statusPayload 5.3%
//
// Both default OFF so the deployed binary behaves exactly like the previous
// one until an experiment turns them on.

var (
	// optQbitSnapshot: build the qBittorrent listing once and share it for a
	// couple of seconds. OFF rebuilds a map per torrent per request, for every
	// caller: 107k maps each time Sonarr, Radarr, cross-seed or autobrr asks.
	// Their polling frequency is not ours to control, which is what makes this
	// worth caching at all.
	optQbitSnapshot atomic.Bool

	// optTotalsCache: memoise the cumulative UL/DL totals for one second.
	// OFF rescans the torrent set on every call, and the SSE pusher calls them
	// on every tick.
	optTotalsCache atomic.Bool
)

// Flag names accepted by /api/opt/flags.
const (
	FlagQbitSnapshot = "qbit_snapshot"
	FlagTotalsCache  = "totals_cache"
)

func init() {
	optQbitSnapshot.Store(false)
	optTotalsCache.Store(false)
}

// SetOptFlag turns one HTTP-side optimisation on or off. Returns false if the
// name is unknown so the caller can try the engine-side registry.
func SetOptFlag(name string, on bool) bool {
	switch name {
	case FlagQbitSnapshot:
		optQbitSnapshot.Store(on)
	case FlagTotalsCache:
		optTotalsCache.Store(on)
	default:
		return false
	}
	return true
}

// OptFlags reports the state of every HTTP-side flag.
func OptFlags() map[string]bool {
	return map[string]bool{
		FlagQbitSnapshot: optQbitSnapshot.Load(),
		FlagTotalsCache:  optTotalsCache.Load(),
	}
}

// ---------------------------------------------------------------------------
// GC tuning
// ---------------------------------------------------------------------------

// gogcCurrent tracks what we last set, since runtime/debug offers no getter
// that does not also change the value. 100 is the Go default and no GOGC is
// set in the environment.
var gogcCurrent atomic.Int64

func init() { gogcCurrent.Store(100) }

// SetGOGC changes the GC target percentage at runtime and returns the previous
// value. Raising it trades resident memory for CPU: with a 637MB live heap and
// ~170MB/s of allocation churn, GOGC=100 collects every four seconds or so.
//
// Careful on this box: the heap it stops reclaiming is memory the ZFS ARC would
// otherwise hold, and the hoard tier reads through the ARC.
func SetGOGC(percent int) int {
	prev := debug.SetGCPercent(percent)
	gogcCurrent.Store(int64(percent))
	return prev
}

// GOGC reports the last value set through SetGOGC.
func GOGC() int64 { return gogcCurrent.Load() }

// ---------------------------------------------------------------------------
// Shared qBittorrent listing snapshot
// ---------------------------------------------------------------------------

const qbitSnapTTL = 2 * time.Second

var (
	qbitSnapMu sync.Mutex
	qbitSnap   []map[string]interface{}
	qbitSnapAt time.Time
)

// qbitSnapshot returns the full qBittorrent listing, rebuilding it via build
// only when the cached copy has expired.
//
// It hands back a COPY OF THE SLICE, sharing the underlying maps. The caller
// filters, sorts in place and paginates; sorting a shared slice from two
// concurrent requests would race, whereas the maps themselves are only ever
// read once built. Copying 107k pointers costs about 1.7MB against the ~68GB/h
// that rebuilding the maps was allocating.
func qbitSnapshot(build func() []map[string]interface{}) []map[string]interface{} {
	if !optQbitSnapshot.Load() {
		return build()
	}
	qbitSnapMu.Lock()
	defer qbitSnapMu.Unlock()
	if qbitSnap == nil || time.Since(qbitSnapAt) > qbitSnapTTL {
		qbitSnap = build()
		qbitSnapAt = time.Now()
	}
	out := make([]map[string]interface{}, len(qbitSnap))
	copy(out, qbitSnap)
	return out
}

// InvalidateQbitSnapshot drops the cached listing. Called when a mutation makes
// the two-second window visibly wrong (an add or a delete through the shim).
func InvalidateQbitSnapshot() {
	qbitSnapMu.Lock()
	qbitSnap = nil
	qbitSnapMu.Unlock()
}

// ---------------------------------------------------------------------------
// Totals memoisation
// ---------------------------------------------------------------------------

const totalsTTL = time.Second

type totalsMemo struct {
	mu     sync.Mutex
	ul, dl int64
	at     time.Time
}

// get returns the memoised pair, refreshing it through f when stale. With the
// flag off it is a straight pass-through, which is what makes the OFF rung of
// the ladder a real baseline.
func (m *totalsMemo) get(f func() (int64, int64)) (int64, int64) {
	if !optTotalsCache.Load() {
		return f()
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	if !m.at.IsZero() && time.Since(m.at) < totalsTTL {
		return m.ul, m.dl
	}
	m.ul, m.dl = f()
	m.at = time.Now()
	return m.ul, m.dl
}

var (
	memoHoardDelta totalsMemo
	memoRain       totalsMemo
	memoGlobal     totalsMemo
)
