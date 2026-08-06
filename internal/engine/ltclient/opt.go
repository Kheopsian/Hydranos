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
// quadratic rescanning and cannot cost anything. list_cache stays OFF — a TTL
// cache never fires when the callers tick 10s apart; sharing one snapshot
// between them needs to be explicit, which is a different change.

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
	optListCache.Store(false)
}

// Flag names as accepted by the API. Kept in one place so the endpoint, the
// logs and the docs cannot drift apart.
const (
	FlagFrame     = "ipc_frame"
	FlagRoute     = "ipc_route"
	FlagListCache = "list_cache"
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
	}
}

// ListCacheTTL bounds how stale a shared list snapshot may be. The callers are
// schedulers running on 10s+ ticks over 100k torrents; a couple of seconds of
// staleness cannot change a slot decision, and the flag is off by default.
const ListCacheTTL = 3 * 1000 * 1000 * 1000 // 3s in nanoseconds
