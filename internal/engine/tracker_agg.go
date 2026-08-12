package engine

// TrackerAgg is a per-tracker rollup of the torrents announcing to one tracker
// host, for a single engine. Built by walking the engine's cached stats under
// its read lock — no full-list copy, so it stays cheap even at 100k+ torrents.
type TrackerAgg struct {
	Tracker       string `json:"tracker"`
	UploadRate    int64  `json:"upload_rate"`
	DownloadRate  int64  `json:"download_rate"`
	Peers         int    `json:"peers"`
	Active        int    `json:"active"`   // torrents currently moving bytes or with peers
	Torrents      int    `json:"torrents"` // total torrents on this tracker
	CumUploaded   int64  `json:"cum_uploaded"`
	CumDownloaded int64  `json:"cum_downloaded"`
}

// accumulateTracker folds one torrent's stats into the per-tracker map.
func accumulateTracker(m map[string]*TrackerAgg, s *TorrentStats) {
	host := s.TrackerHost
	if host == "" {
		host = "(none)"
	}
	a := m[host]
	if a == nil {
		a = &TrackerAgg{Tracker: host}
		m[host] = a
	}
	a.Torrents++
	a.UploadRate += s.UploadRate
	a.DownloadRate += s.DownloadRate
	a.Peers += s.NumPeers
	a.CumUploaded += s.TotalUpload
	a.CumDownloaded += s.TotalDownload
	if s.UploadRate > 0 || s.DownloadRate > 0 || s.NumPeers > 0 {
		a.Active++
	}
}

// AggregateByTracker rolls up the race engine's torrents by tracker host.
func (e *RaceEngine) AggregateByTracker() map[string]*TrackerAgg {
	m := make(map[string]*TrackerAgg)
	e.cachedStatsMu.RLock()
	for i := range e.cachedTorrentList {
		accumulateTracker(m, &e.cachedTorrentList[i])
	}
	e.cachedStatsMu.RUnlock()
	return m
}

// AggregateByTracker rolls up the hoard engine's torrents by tracker host.
func (e *HoardEngine) AggregateByTracker() map[string]*TrackerAgg {
	m := make(map[string]*TrackerAgg)
	e.cachedStatsMu.RLock()
	for _, s := range e.cachedStats {
		accumulateTracker(m, s)
	}
	e.cachedStatsMu.RUnlock()
	return m
}

// TrackerHostFor returns the tracker host cached for a torrent, or "" if the
// torrent is unknown. Used at remove time to attribute a departing torrent's
// carried-over stats to the right tracker.
func (e *RaceEngine) TrackerHostFor(infoHash string) string {
	e.cachedStatsMu.RLock()
	defer e.cachedStatsMu.RUnlock()
	if s := e.cachedStats[infoHash]; s != nil {
		return s.TrackerHost
	}
	return ""
}

func (e *HoardEngine) TrackerHostFor(infoHash string) string {
	e.cachedStatsMu.RLock()
	defer e.cachedStatsMu.RUnlock()
	if s := e.cachedStats[infoHash]; s != nil {
		return s.TrackerHost
	}
	return ""
}

// AllTotals returns every torrent's lifetime {UL, DL} keyed by info hash, folded
// under a single read lock like AggregateByTracker — no full-list copy, so it
// stays cheap at 100k+ torrents. Feeds the periodic store sync, which is what
// gives the per-torrent counters somewhere durable to live.
func (e *RaceEngine) AllTotals() map[string][2]int64 {
	e.cachedStatsMu.RLock()
	defer e.cachedStatsMu.RUnlock()
	out := make(map[string][2]int64, len(e.cachedTorrentList))
	for i := range e.cachedTorrentList {
		s := &e.cachedTorrentList[i]
		out[s.InfoHash] = [2]int64{s.TotalUpload, s.TotalDownload}
	}
	return out
}

// AllTotals mirrors the race engine's, over the hoard engine's cached stats.
func (e *HoardEngine) AllTotals() map[string][2]int64 {
	e.cachedStatsMu.RLock()
	defer e.cachedStatsMu.RUnlock()
	out := make(map[string][2]int64, len(e.cachedStats))
	for ih, s := range e.cachedStats {
		if s == nil {
			continue
		}
		out[ih] = [2]int64{s.TotalUpload, s.TotalDownload}
	}
	return out
}
