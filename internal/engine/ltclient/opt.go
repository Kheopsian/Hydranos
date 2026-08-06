package ltclient

import "sync/atomic"

// ---------------------------------------------------------------------------
// Hot-swappable optimisation flags for the Go<->engine IPC path.
// ---------------------------------------------------------------------------
//
// Added 2026-08-05 after a prod CPU profile showed readLoop burning ~30-37% of
// the Go process. Each flag gates ONE optimisation so it can be toggled at
// runtime (POST /api/opt/flags) and measured in isolation with pprof, without
// restarting: a restart resets the per-torrent upload counters, and the tracker
// credits upload by MAX per torrent, so restarting to A/B costs real credit.
//
// OFF reproduces the pre-3.42.4 behaviour on purpose — that is what makes the
// baseline rung of the ladder a real baseline rather than a memory of one.
//
// Measured on prod 2026-08-05, each flag alone, 600s windows, 107k torrents
// (drift between two identical OFF windows: ~9%, so anything under that is not
// a claim):
//
//	ipc_route   74.7% CPU vs 87.6-95.7% OFF   json.Unmarshal 220s -> 120s
//	ipc_frame   96.1% CPU (no total gain)     indexbytebody   27s -> 0
//	list_cache  87.7% CPU = inside the noise; cachedList never sampled
//
// So ipc_route carries the win and ships ON. ipc_frame ships ON too: its effect
// on the total is below the noise floor, but it provably deletes ~27s/600s of
// quadratic rescanning and cannot cost anything.
//
// list_cache measured as exactly nothing at the time, and the reason turned out
// to be a bug rather than a property: its TTL was a 3s constant while the three
// schedulers that would use it tick at 10s, 30s and 60s, so every caller always
// found the snapshot expired. With the TTL adjustable and set just under the
// fastest ticker it fires, and it ships ON as of 3.44.0.
//
// Measured again on prod 2026-08-06, 900s windows, 107k torrents, states
// interleaved A/B/C twice (drift between two identical A windows: 3.0%):
//
//	A  all off, GOGC 100      go 67.4%   RSS peak 2873MB
//	B  four flags on          go 39.8%   RSS peak 2821MB   at HIGHER load
//	C  B plus GOGC 200        go 33.2%   RSS peak 5864MB
//
// B is the configuration that ships: -41% on the control plane at no cost in
// memory, and it also took 17% off the Rust hoard engine, which nobody had
// predicted — fewer list_torrents calls means the engine serialises its ~100MB
// reply less often, so the saving lands on both sides of the IPC.
//
// C is not worth it and does not ship: 6.6 points of CPU for three extra
// gigabytes of resident memory. The ZFS ARC turned out to be untouched (51.5GB
// throughout, refuting the concern that a fatter heap would starve it), but
// GOMEMLIMIT is 8GiB and C already peaked at 6.0GB — at a larger catalogue that
// headroom disappears and the collector would start thrashing. GOGC stays a
// knob, defaulting to the Go default of 100.

var (
	// optFrame: assemble a socket frame in ONE pass over the bytes.
	// OFF re-scans the whole accumulated frame on every buffer refill, which is
	// what bufio.Scanner does internally — quadratic in frame size (~100MB for
	// list_torrents at 107k torrents; profile: indexbytebody 14.7-20.3%).
	optFrame atomic.Bool

	// optRoute: route a frame with a SINGLE header-only decode.
	// OFF parses the full frame up to three times before the caller's real
	// decode: once into map[string]json.RawMessage to test for "event", once
	// into Response for the id, once more in callVia for the error field.
	optRoute atomic.Bool

	// optPrealloc: size the frame buffer from the largest frame seen so far.
	// OFF grows it by repeated append, which for a ~100MB list_torrents frame
	// walks the doubling cascade and allocates roughly twice the frame size in
	// throwaway intermediates. readFrame is 37% of all bytes allocated by the
	// process, and the GC bill follows the bytes.
	optPrealloc atomic.Bool

	// optListCache: serve list_torrents from a short-lived shared snapshot.
	// OFF lets every caller (hoardScheduler.reconcile 66%, enforceDownloadSlots
	// 22%, refreshStats 11%) pull and decode its own copy of all torrents.
	optListCache atomic.Bool
)

func init() {
	// Defaults, per the measurements above. Still hot-swappable: turning one
	// OFF restores the old code path exactly, which is the rollback for a
	// regression that no restart can beat for speed.
	optFrame.Store(true)
	optRoute.Store(true)
	optListCache.Store(true)
	optPrealloc.Store(true)
	listCacheTTL.Store(9 * 1000 * 1000 * 1000) // 9s: just under the 10s ticker
}

// Flag names as accepted by the API. Kept in one place so the endpoint, the
// logs and the docs cannot drift apart.
const (
	FlagFrame     = "ipc_frame"
	FlagRoute     = "ipc_route"
	FlagListCache = "list_cache"
	FlagPrealloc  = "ipc_prealloc"
)

// SetOptFlag turns one optimisation on or off. Returns false if the name is
// unknown, so the API can answer 400 instead of silently doing nothing.
func SetOptFlag(name string, on bool) bool {
	switch name {
	case FlagFrame:
		optFrame.Store(on)
	case FlagRoute:
		optRoute.Store(on)
	case FlagListCache:
		optListCache.Store(on)
	case FlagPrealloc:
		optPrealloc.Store(on)
	default:
		return false
	}
	return true
}

// OptFlags reports the current state of every flag.
func OptFlags() map[string]bool {
	return map[string]bool{
		FlagFrame:     optFrame.Load(),
		FlagRoute:     optRoute.Load(),
		FlagListCache: optListCache.Load(),
		FlagPrealloc:  optPrealloc.Load(),
	}
}

// listCacheTTL bounds how stale a shared list snapshot may be, in nanoseconds.
//
// It is a variable, not a constant, because the original 3s could never fire:
// the three schedulers tick at 10s, 30s and 60s, so every caller always found
// the snapshot expired and decoded its own copy. Measured effect of the flag
// was consequently zero. A TTL just under the fastest ticker lets the 30s and
// 60s callers ride on the 10s one. The callers are schedulers over 100k
// torrents; a few seconds of staleness cannot change a slot decision.
var listCacheTTL atomic.Int64

// ListCacheTTL reports the current snapshot TTL as a duration in nanoseconds.
func ListCacheTTL() int64 { return listCacheTTL.Load() }

// SetListCacheTTL sets the snapshot TTL in nanoseconds. Values below zero are
// ignored; zero disables reuse without touching the flag.
func SetListCacheTTL(ns int64) bool {
	if ns < 0 {
		return false
	}
	listCacheTTL.Store(ns)
	return true
}
