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
