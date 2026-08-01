package api

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"sync"
)

// Per-(engine, tracker) monotone carry-over. When a torrent is removed its
// lifetime UL/DL is folded here so a tracker's cumulative never drops just
// because a torrent was deleted. Mirrors the global baseline (AbsorbStats), but
// keyed by tracker. cum shown = baseline[engine,tracker] + Σ(live torrents).
var (
	trackerBaselineMu   sync.Mutex
	trackerBaselineUL   = map[string]int64{} // key = engine + "\x00" + tracker
	trackerBaselineDL   = map[string]int64{}
	trackerBaselineFile string
)

func trackerKey(engine, tracker string) string { return engine + "\x00" + tracker }

// AbsorbTrackerStats folds a removed torrent's lifetime UL/DL into the
// per-(engine, tracker) baseline. Called from the OnBeforeRemove hooks.
func AbsorbTrackerStats(engine, tracker string, ul, dl int64) {
	if ul <= 0 && dl <= 0 {
		return
	}
	if tracker == "" {
		tracker = "(none)"
	}
	k := trackerKey(engine, tracker)
	trackerBaselineMu.Lock()
	if ul > 0 {
		trackerBaselineUL[k] += ul
	}
	if dl > 0 {
		trackerBaselineDL[k] += dl
	}
	trackerBaselineMu.Unlock()
	saveTrackerBaseline()
}

// GetTrackerBaseline returns a snapshot keyed by engine+"\x00"+tracker, value
// {UL, DL}, for the bench sampler to add to live per-tracker sums.
func GetTrackerBaseline() map[string][2]int64 {
	trackerBaselineMu.Lock()
	defer trackerBaselineMu.Unlock()
	out := make(map[string][2]int64, len(trackerBaselineUL))
	for k, ul := range trackerBaselineUL {
		out[k] = [2]int64{ul, trackerBaselineDL[k]}
	}
	for k, dl := range trackerBaselineDL {
		if _, ok := out[k]; !ok {
			out[k] = [2]int64{0, dl}
		}
	}
	return out
}

// trackerBaselineEntry is the on-disk shape of one carry-over row.
type trackerBaselineEntry struct {
	Engine  string `json:"engine"`
	Tracker string `json:"tracker"`
	UL      int64  `json:"ul"`
	DL      int64  `json:"dl"`
}

func initTrackerBaseline(dataDir string) {
	trackerBaselineFile = filepath.Join(dataDir, "baseline_trackers.json")
	data, err := os.ReadFile(trackerBaselineFile)
	if err != nil {
		return
	}
	var entries []trackerBaselineEntry
	if json.Unmarshal(data, &entries) != nil {
		return
	}
	trackerBaselineMu.Lock()
	for _, e := range entries {
		k := trackerKey(e.Engine, e.Tracker)
		trackerBaselineUL[k] = e.UL
		trackerBaselineDL[k] = e.DL
	}
	trackerBaselineMu.Unlock()
}

func saveTrackerBaseline() {
	if trackerBaselineFile == "" {
		return
	}
	trackerBaselineMu.Lock()
	entries := make([]trackerBaselineEntry, 0, len(trackerBaselineUL))
	for k, ul := range trackerBaselineUL {
		eng, trk := k, ""
		if i := strings.IndexByte(k, '\x00'); i >= 0 {
			eng, trk = k[:i], k[i+1:]
		}
		entries = append(entries, trackerBaselineEntry{Engine: eng, Tracker: trk, UL: ul, DL: trackerBaselineDL[k]})
	}
	trackerBaselineMu.Unlock()
	data, err := json.Marshal(entries)
	if err != nil {
		return
	}
	tmp := trackerBaselineFile + ".tmp"
	if os.WriteFile(tmp, data, 0644) == nil {
		os.Rename(tmp, trackerBaselineFile)
	}
}
