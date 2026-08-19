package engine

import "sync"

// cachedTrackerObs is a process-wide map of (info_hash → url → last
// observation), populated by ObserveAnnounce. Kept package-level so we
// don't have to wedge new fields into HoardEngine and risk cross-cutting
// merge pain. RaceEngine reuses HoardAnnouncer and now writes here too, via
// its own ObserveAnnounce: before that its callback was nil, so race torrents
// had no per-tracker data at all and their detail view showed the engine's
// stale placeholders as if they were measurements.
var (
	cachedTrackerObsMu sync.RWMutex
	cachedTrackerObs   = make(map[string]map[string]TrackerObservation)
)

// ObserveAnnounce folds the result of a Go-canonical tracker announce cycle
// into cachedStats + cachedTrackerObs, so /api/status, /api/hoard/torrents
// and /api/hoard/torrents/<ih> see swarm scrape numbers and per-tracker
// errors again. Wired from main.go to HoardAnnouncer.OnObservation. Typhon's
// internal announce loop is disabled (DisableInternalAnnounce=true) so this
// is the only feed that keeps these fields non-zero in the UI.
func (e *HoardEngine) ObserveAnnounce(infoHash string, obs AnnounceObservation) {
	e.cachedStatsMu.Lock()
	st, exists := e.cachedStats[infoHash]
	if !exists {
		st = &TorrentStats{InfoHash: infoHash}
		e.cachedStats[infoHash] = st
	}
	st.IsAnnounced = true
	if obs.OK {
		st.SwarmSeeds = obs.Seeds
		st.SwarmLeechers = obs.Leechers
		st.TrackerError = false
		st.TrackerErrorMsg = ""
	} else {
		st.TrackerError = true
		st.TrackerErrorMsg = obs.ErrorMsg
	}
	e.cachedStatsMu.Unlock()

	if len(obs.Trackers) > 0 {
		cachedTrackerObsMu.Lock()
		cachedTrackerObs[infoHash] = obs.Trackers
		cachedTrackerObsMu.Unlock()
	}
}

// ObserveAnnounce is the race engine's half of the same feed. Typhon's
// internal announce loop is off for BOTH engines (DisableInternalAnnounce is
// set on each), so without this the race panel could only ever show what the
// engine guessed at add time.
func (e *RaceEngine) ObserveAnnounce(infoHash string, obs AnnounceObservation) {
	e.cachedStatsMu.Lock()
	st, exists := e.cachedStats[infoHash]
	if !exists {
		st = &TorrentStats{InfoHash: infoHash}
		e.cachedStats[infoHash] = st
	}
	st.IsAnnounced = true
	if obs.OK {
		st.SwarmSeeds = obs.Seeds
		st.SwarmLeechers = obs.Leechers
		st.TrackerError = false
		st.TrackerErrorMsg = ""
	} else {
		st.TrackerError = true
		st.TrackerErrorMsg = obs.ErrorMsg
	}
	e.cachedStatsMu.Unlock()

	if len(obs.Trackers) > 0 {
		cachedTrackerObsMu.Lock()
		cachedTrackerObs[infoHash] = obs.Trackers
		cachedTrackerObsMu.Unlock()
	}
}

// trackerObsFor returns a snapshot of per-tracker observations for the
// given info_hash, or nil if none. Read by GetTorrentDetail to overwrite
// Typhon's stale endpoints[] in the trackers passthrough.
func trackerObsFor(infoHash string) map[string]TrackerObservation {
	cachedTrackerObsMu.RLock()
	defer cachedTrackerObsMu.RUnlock()
	src, ok := cachedTrackerObs[infoHash]
	if !ok {
		return nil
	}
	out := make(map[string]TrackerObservation, len(src))
	for k, v := range src {
		out[k] = v
	}
	return out
}

// forgetTrackerObs drops the per-tracker cache for a removed torrent.
func forgetTrackerObs(infoHash string) {
	cachedTrackerObsMu.Lock()
	delete(cachedTrackerObs, infoHash)
	cachedTrackerObsMu.Unlock()
}
