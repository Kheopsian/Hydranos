package api

import (
	"encoding/json"
	"testing"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

func seededDeltaServer() *Server {
	s := &Server{}
	s.agentRows.byAgent = map[string]rowSet{
		"seedbox": {
			rowKey("hoard-0", "aa"): {"info_hash": "aa", "name": "A", "agent_engine": "hoard-0",
				"upload_rate": int64(0), "num_peers": 0},
		},
	}
	return s
}

func snapshotEvent(hash string, ul int64, peers int) ltclient.Event {
	d, _ := json.Marshal(ltclient.StatsSnapshotData{Torrents: []ltclient.TorrentStatsMini{{
		InfoHash: hash, UploadRate: ul, PeersConnected: peers, TotalUploaded: 100, TotalDownloaded: 50,
	}}})
	return ltclient.Event{Type: "stats_snapshot", Data: d}
}

// A delta must update the row in place. This is the entire point: it replaces a
// full re-listing that costs 209 ms and 271 MB at production scale.
func TestStatsSnapshotUpdatesTheRowInPlace(t *testing.T) {
	s := seededDeltaServer()
	if n := s.applyStatsSnapshot("seedbox", "hoard-0", snapshotEvent("AA", 999, 7).Data); n != 1 {
		t.Fatalf("touched %d row(s), want 1: the delta matched nothing, so the stream is doing no work", n)
	}
	row := s.agentRows.byAgent["seedbox"][rowKey("hoard-0", "aa")]
	if row["upload_rate"] != int64(999) || row["num_peers"] != 7 {
		t.Errorf("row not updated: %v", row)
	}
	// Static metadata must survive: the snapshot does not carry it, and losing
	// it would blank the name in the table.
	if row["name"] != "A" {
		t.Errorf("static metadata lost: name = %v", row["name"])
	}
}

// A snapshot must NEVER create a row. It carries no name, no save path and no
// category, so an invented row shows in the table as a blank line that never
// fills in.
func TestStatsSnapshotNeverInventsARow(t *testing.T) {
	s := seededDeltaServer()
	before := len(s.agentRows.byAgent["seedbox"])
	s.applyStatsSnapshot("seedbox", "hoard-0", snapshotEvent("zz", 1, 1).Data)
	if after := len(s.agentRows.byAgent["seedbox"]); after != before {
		t.Errorf("row count went %d -> %d: a nameless row was added to the table", before, after)
	}
}

func TestTorrentRemovedDropsTheRow(t *testing.T) {
	s := seededDeltaServer()
	d, _ := json.Marshal(ltclient.TorrentRemovedData{InfoHash: "AA"})
	if !s.applyTorrentRemoved("seedbox", "hoard-0", d) {
		t.Fatal("removal reported nothing removed")
	}
	if len(s.agentRows.byAgent["seedbox"]) != 0 {
		t.Error("the row survived its own removal event")
	}
	// A second one is a no-op, not a panic or a false positive.
	if s.applyTorrentRemoved("seedbox", "hoard-0", d) {
		t.Error("removing an absent row reported success")
	}
}

// An add cannot be applied -- it does not carry a whole row -- so it must ask
// for a refresh instead of being dropped, or the torrent never appears.
func TestAddAsksForARefreshInsteadOfBeingSwallowed(t *testing.T) {
	s := seededDeltaServer()
	for _, typ := range []string{"torrent_added", "torrent_completed", "torrent_error"} {
		if !s.applyAgentEvent("seedbox", "hoard-0", ltclient.Event{Type: typ}) {
			t.Errorf("%s did not ask for a refresh: the change would never reach the table", typ)
		}
	}
	if s.applyAgentEvent("seedbox", "hoard-0", snapshotEvent("aa", 1, 1)) {
		t.Error("a stats delta asked for a full refresh, which is exactly what the stream exists to avoid")
	}
}

// Events for an agent with no cache yet, or malformed payloads, must not panic:
// the stream is live before the first poll has filled anything.
func TestDeltaOnAnUnknownAgentIsHarmless(t *testing.T) {
	s := &Server{}
	if n := s.applyStatsSnapshot("nobody", "hoard-0", snapshotEvent("aa", 1, 1).Data); n != 0 {
		t.Errorf("touched %d rows on an empty cache", n)
	}
	if s.applyTorrentRemoved("nobody", "hoard-0", json.RawMessage(`{"info_hash":"aa"}`)) {
		t.Error("removed something from an empty cache")
	}
	if n := s.applyStatsSnapshot("seedbox", "hoard-0", json.RawMessage(`not json`)); n != 0 {
		t.Error("malformed payload was applied")
	}
}
