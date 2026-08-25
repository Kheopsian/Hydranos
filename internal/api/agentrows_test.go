package api

import (
	"testing"
	"time"
)

func seededServer() *Server {
	s := &Server{}
	s.agentRows.byAgent = map[string]rowSet{
		"seedbox": {
			"aa": {"info_hash": "aa", "agent": "seedbox", "agent_engine": "hoard",
				"num_peers": 3, "upload_rate": int64(120), "download_rate": int64(0),
				"swarm_leechers": 10, "tracker_error": false},
			"bb": {"info_hash": "bb", "agent": "seedbox", "agent_engine": "hoard2",
				"num_peers": 0, "upload_rate": int64(0), "download_rate": int64(50),
				"swarm_leechers": 4, "tracker_error": true},
		},
	}
	s.agentRows.at = time.Now()
	return s
}

// The hoard table is fed by SSE alone, so the hydration has to carry the
// agents' rows or a torrent placed on an agent has no row at all.
func TestAgentHoardRowsServesCache(t *testing.T) {
	s := seededServer()
	rows := s.agentHoardRows()
	if len(rows) != 2 {
		t.Fatalf("got %d rows, want 2", len(rows))
	}
	for _, r := range rows {
		if r["agent"] != "seedbox" {
			t.Fatalf("row is missing its agent: %v", r)
		}
	}
}

// A stale cache must refresh rather than serve rows frozen from whenever the
// last browser was connected. With no agents dialed that yields an empty set.
func TestAgentHoardRowsRefreshesWhenStale(t *testing.T) {
	s := seededServer()
	s.agentRows.at = time.Now().Add(-time.Hour)
	if rows := s.agentHoardRows(); len(rows) != 0 {
		t.Fatalf("got %d rows from a stale cache with no agents, want 0", len(rows))
	}
}

func TestAgentHoardOwner(t *testing.T) {
	s := seededServer()
	agent, engineID, ok := s.agentHoardOwner("bb")
	if !ok || agent != "seedbox" || engineID != "hoard2" {
		t.Fatalf("owner of bb = (%q, %q, %v), want (seedbox, hoard2, true)", agent, engineID, ok)
	}
	if _, _, ok := s.agentHoardOwner("zz"); ok {
		t.Fatal("unknown hash reported as owned")
	}
}

// The header sits directly above the table: it has to count the same torrents.
func TestMergeAgentHoardStats(t *testing.T) {
	s := seededServer()
	status := map[string]interface{}{
		"total_torrents": 5, "torrents_with_peers": 1, "torrents_uploading": 1,
		"torrents_announced": 5, "active_peers": 2, "swarm_leechers": 7,
		"active_upload_rate": int64(1000), "active_download_rate": int64(0),
	}
	s.mergeAgentHoardStats(status)

	for key, want := range map[string]int64{
		"total_torrents":       7,    // 5 local + 2 on the agent
		"torrents_with_peers":  2,    // one agent row has peers
		"torrents_uploading":   2,    // one agent row is uploading
		"torrents_announced":   6,    // the tracker-erroring row does not count
		"active_peers":         5,    // 2 + 3
		"swarm_leechers":       21,   // 7 + 10 + 4
		"active_upload_rate":   1120, // 1000 + 120
		"active_download_rate": 50,
	} {
		if got := numOf(status[key]); got != want {
			t.Errorf("%s = %d, want %d", key, got, want)
		}
	}
}

// Session and baseline totals feed announce accounting and stay node-local.
func TestMergeAgentHoardStatsLeavesSessionTotals(t *testing.T) {
	s := seededServer()
	status := map[string]interface{}{"session_uploaded": int64(42), "session_downloaded": int64(7)}
	s.mergeAgentHoardStats(status)
	if numOf(status["session_uploaded"]) != 42 || numOf(status["session_downloaded"]) != 7 {
		t.Fatalf("session totals were touched: %v", status)
	}
}

func TestMergeAgentHoardStatsNoAgents(t *testing.T) {
	s := &Server{}
	status := map[string]interface{}{"total_torrents": 3}
	s.mergeAgentHoardStats(status)
	if numOf(status["total_torrents"]) != 3 {
		t.Fatalf("total_torrents = %v, want 3 untouched", status["total_torrents"])
	}
}

// A front-only node has no engine, but the UI is fed over /api/events and that
// handler refuses to serve without a hub: nil left the whole interface empty.
func TestFrontOnlyHoardEngineHasEventHub(t *testing.T) {
	if NewEmptyHoardEngine().EventHub() == nil {
		t.Fatal("front-only hoard stub has no event hub: /api/events would 503 and the UI stays blank")
	}
}
