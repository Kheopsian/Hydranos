package engine

import (
	"net/url"
	"sort"
	"strings"
	"sync"
	"time"
)

// TrackerStat is the per-host aggregate exposed to the Trackers UI tab. It is
// built from live announce results (see recordAnnounce, called from
// announceAllTiers) so it reflects the trackers we are ACTUALLY announcing to —
// i.e. the hot set of announceable torrents, not the cold/paused hoard. That
// keeps its memory footprint O(hot), aligned with the 1M-torrent scaling work.
type TrackerStat struct {
	Host         string    `json:"host"`
	Torrents     int       `json:"torrents"`      // distinct torrents that announced here recently
	OK           bool      `json:"ok"`            // last announce to this host succeeded
	LastError    string    `json:"last_error"`    // last failure reason (empty when OK)
	LastAnnounce time.Time `json:"last_announce"` // time of the most recent announce
	Announces    int64     `json:"announces"`     // cumulative successful announces since boot
	Errors       int64     `json:"errors"`        // cumulative failed announces since boot
}

// trackerStalePrune drops a torrent from a host's active set once it hasn't
// re-announced within this window (removed, paused, or moved tracker). Kept a
// bit above the default announce interval so a healthy torrent never flickers
// out of the count between two announces.
const trackerStalePrune = 90 * time.Minute

type hostAgg struct {
	seen      map[string]time.Time // info_hash -> last announce time
	ok        bool
	lastError string
	lastAt    time.Time
	announces int64
	errors    int64
}

type trackerRegistry struct {
	mu    sync.Mutex
	hosts map[string]*hostAgg
}

// trackerReg is the process-wide announce registry. It aggregates across every
// engine (race + hoard + shards) since they all announce through a
// HoardAnnouncer, giving a single "our trackers" view.
var trackerReg = &trackerRegistry{hosts: make(map[string]*hostAgg)}

// recordAnnounce folds one (torrent, tracker) announce result into the per-host
// aggregate. Called for every HTTP(S) tracker tried in announceAllTiers.
func (r *trackerRegistry) recordAnnounce(host, infoHash string, ok bool, errMsg string) {
	if host == "" {
		return
	}
	now := time.Now()
	r.mu.Lock()
	defer r.mu.Unlock()
	a := r.hosts[host]
	if a == nil {
		a = &hostAgg{seen: make(map[string]time.Time)}
		r.hosts[host] = a
	}
	a.seen[infoHash] = now
	a.lastAt = now
	a.ok = ok
	if ok {
		a.announces++
		a.lastError = ""
	} else {
		a.errors++
		if errMsg != "" {
			a.lastError = errMsg
		}
	}
}

// snapshot returns the current per-host aggregates, pruning torrents that
// stopped announcing. Sorted by torrent count desc so the busiest trackers
// surface first.
func (r *trackerRegistry) snapshot() []TrackerStat {
	cutoff := time.Now().Add(-trackerStalePrune)
	r.mu.Lock()
	out := make([]TrackerStat, 0, len(r.hosts))
	for host, a := range r.hosts {
		for ih, t := range a.seen {
			if t.Before(cutoff) {
				delete(a.seen, ih)
			}
		}
		if len(a.seen) == 0 && a.lastAt.Before(cutoff) {
			delete(r.hosts, host)
			continue
		}
		out = append(out, TrackerStat{
			Host:         host,
			Torrents:     len(a.seen),
			OK:           a.ok,
			LastError:    a.lastError,
			LastAnnounce: a.lastAt,
			Announces:    a.announces,
			Errors:       a.errors,
		})
	}
	r.mu.Unlock()
	sort.Slice(out, func(i, j int) bool {
		if out[i].Torrents != out[j].Torrents {
			return out[i].Torrents > out[j].Torrents
		}
		return out[i].Host < out[j].Host
	})
	return out
}

// TrackerSnapshot exposes the per-host announce aggregate for the API layer.
func TrackerSnapshot() []TrackerStat {
	return trackerReg.snapshot()
}

// trackerHostOf extracts the bare hostname (no port) from a tracker announce
// URL, e.g. "https://www.myanonamouse.net/tracker/announce" -> "www.myanonamouse.net".
func trackerHostOf(rawurl string) string {
	u, err := url.Parse(rawurl)
	if err != nil || u.Hostname() == "" {
		return strings.TrimSpace(rawurl)
	}
	return u.Hostname()
}

// ClientSpoofForHost returns the client spoof that would apply to a tracker on
// the given host, if any (mirrors clientOverrideFor's substring matching).
func ClientSpoofForHost(host string) (ClientSpoof, bool) {
	return clientOverrideFor("http://" + host + "/announce")
}

// PasskeyOverrideForHost reports whether a passkey override would apply to the
// given host (same host-substring matching as applyPasskeyOverride).
func PasskeyOverrideForHost(host string) bool {
	passkeyOverrideMu.RLock()
	defer passkeyOverrideMu.RUnlock()
	for h := range passkeyOverrides {
		if strings.Contains(host, h) {
			return true
		}
	}
	return false
}
