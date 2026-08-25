package api

import (
	"encoding/json"
	"strings"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// Applying an agent's event stream to the row cache, instead of re-listing it.
//
// The engine emits stats_snapshot roughly once a second carrying ONLY the
// torrents whose counters moved, which on a mostly-idle six-figure library is a
// handful of entries. Re-listing to learn the same thing costs 209 ms and
// 271 MB at 198k torrents (rowcache_bench_test.go) and produces an identical
// answer, so the stream is strictly better whenever it is available.
//
// What each event can and cannot do here matters:
//
//   - stats_snapshot carries the DYNAMIC fields only (rates, totals, peers).
//     It updates rows that already exist and must never create one: it has no
//     name, no save path, no category, so a row invented from it would appear
//     in the table as a blank line that never fills in.
//   - torrent_removed deletes, and is the only event that may.
//   - torrent_added cannot be applied at all -- it does not carry enough to
//     build a row -- so it marks the agent for a refresh instead. Adds are rare;
//     rate changes are not, and those are the ones worth streaming.

// applyStatsSnapshot folds a delta into one agent's rows. Returns how many rows
// it actually touched, which is what tells a caller the stream is doing its job
// rather than silently matching nothing.
func (s *Server) applyStatsSnapshot(agentName, engineID string, raw json.RawMessage) int {
	var data ltclient.StatsSnapshotData
	if err := json.Unmarshal(raw, &data); err != nil || len(data.Torrents) == 0 {
		return 0
	}
	s.agentRows.mu.Lock()
	defer s.agentRows.mu.Unlock()
	set := s.agentRows.byAgent[agentName]
	if set == nil {
		return 0
	}
	n := 0
	for _, t := range data.Torrents {
		row := set[rowKey(engineID, t.InfoHash)]
		if row == nil {
			// Unknown torrent: the snapshot cannot describe it well enough to
			// show, so leave it to the next refresh rather than adding a row
			// with no name.
			continue
		}
		row["upload_rate"] = t.UploadRate
		row["download_rate"] = t.DownloadRate
		row["total_uploaded"] = t.TotalUploaded
		row["total_downloaded"] = t.TotalDownloaded
		row["num_peers"] = t.PeersConnected
		if t.TotalDownloaded > 0 {
			row["ratio"] = float64(t.TotalUploaded) / float64(t.TotalDownloaded)
		}
		n++
	}
	return n
}

// applyTorrentRemoved drops a row. Returns whether it removed anything, so a
// caller can tell a real removal from an event for a torrent this node never
// had a row for.
func (s *Server) applyTorrentRemoved(agentName, engineID string, raw json.RawMessage) bool {
	var data ltclient.TorrentRemovedData
	if err := json.Unmarshal(raw, &data); err != nil || strings.TrimSpace(data.InfoHash) == "" {
		return false
	}
	s.agentRows.mu.Lock()
	defer s.agentRows.mu.Unlock()
	set := s.agentRows.byAgent[agentName]
	if set == nil {
		return false
	}
	k := rowKey(engineID, data.InfoHash)
	if _, ok := set[k]; !ok {
		return false
	}
	delete(set, k)
	return true
}

// applyAgentEvent routes one decoded event. The boolean says whether the caller
// should schedule a refresh, which is how an add gets its row without this file
// having to invent one.
func (s *Server) applyAgentEvent(agentName, engineID string, ev ltclient.Event) (needsRefresh bool) {
	switch ev.Type {
	case "stats_snapshot":
		s.applyStatsSnapshot(agentName, engineID, ev.Data)
		return false
	case "torrent_removed":
		s.applyTorrentRemoved(agentName, engineID, ev.Data)
		return false
	case "torrent_added", "torrent_completed", "torrent_error":
		// These change what a row SAYS beyond its counters -- state, error
		// text, completion time -- and none of them carries the whole row.
		// Rare enough that a refresh is the honest way to pick them up.
		return true
	}
	return false
}
