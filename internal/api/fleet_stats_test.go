package api

import (
	"testing"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
	"github.com/gin-gonic/gin"
)

// A controller's local race engine contributes nothing, so the payload has no
// race section at all until the agents put one there.
func TestAddAgentRaceStatsBuildsTheSectionOnAController(t *testing.T) {
	s := &Server{}
	s.fleet.race = raceAgg{torrents: 3, withPeers: 2, peers: 40, activeDL: 1, activeSeeds: 2, ulRate: 500, dlRate: 900}

	out := gin.H{}
	s.addAgentRaceStats(out)

	race, ok := out["race"].(map[string]interface{})
	if !ok {
		t.Fatalf("no race section: %v", out)
	}
	if race["torrents"] != 3 || race["total_peers"] != 40 || race["active_downloads"] != 1 {
		t.Fatalf("race section = %v", race)
	}
	if race["total_upload_rate"] != int64(500) {
		t.Errorf("total_upload_rate = %v (%T), want int64 500", race["total_upload_rate"], race["total_upload_rate"])
	}
}

// A monolith that also drives agents runs both, and the overview is the one
// place that has to show all of it.
func TestAddAgentRaceStatsAddsToTheLocalEngine(t *testing.T) {
	s := &Server{}
	s.fleet.race = raceAgg{torrents: 2, peers: 5, ulRate: 100}
	out := gin.H{"race": map[string]interface{}{
		"torrents": 10, "total_peers": 20, "total_upload_rate": int64(900),
	}}
	s.addAgentRaceStats(out)

	race := out["race"].(map[string]interface{})
	if race["torrents"] != 12 || race["total_peers"] != 25 {
		t.Fatalf("counts did not add: %v", race)
	}
	if race["total_upload_rate"] != int64(1000) {
		t.Fatalf("rate did not add: %v", race["total_upload_rate"])
	}
}

func TestAddAgentRaceStatsLeavesAnEmptyFleetAlone(t *testing.T) {
	s := &Server{}
	out := gin.H{}
	s.addAgentRaceStats(out)
	if _, ok := out["race"]; ok {
		t.Error("invented a race section with no agents behind it")
	}
}

func TestRaceAggCountsByState(t *testing.T) {
	var a raceAgg
	a.add(ltclient.TorrentStatus{State: "downloading", NumPeers: 3, UploadRate: 1, DownloadRate: 2})
	a.add(ltclient.TorrentStatus{State: "seeding", NumPeers: 0, UploadRate: 5})
	a.add(ltclient.TorrentStatus{State: "paused"})
	if a.torrents != 3 || a.activeDL != 1 || a.activeSeeds != 1 {
		t.Fatalf("agg = %+v", a)
	}
	if a.withPeers != 1 || a.peers != 3 {
		t.Fatalf("peer counts = %+v", a)
	}
	if a.ulRate != 6 || a.dlRate != 2 {
		t.Fatalf("rates = %+v", a)
	}
}

// Three agents out of four answering is not the fleet shrinking by a quarter.
// Publishing that sample makes the overview dip on any one slow agent, which
// is the flicker keeping a last-good sample exists to prevent.
func TestPublishRaceAggIgnoresAPartialPoll(t *testing.T) {
	s := &Server{}
	s.publishRaceAgg(raceAgg{torrents: 100, peers: 400}, 4, 4)
	if s.fleet.race.torrents != 100 {
		t.Fatalf("a complete poll was not published: %+v", s.fleet.race)
	}
	at := s.fleet.at

	s.publishRaceAgg(raceAgg{torrents: 75, peers: 300}, 4, 3)
	if s.fleet.race.torrents != 100 {
		t.Fatalf("a partial poll replaced the last good sample: %+v", s.fleet.race)
	}
	if !s.fleet.at.Equal(at) {
		t.Fatal("a partial poll refreshed the sample time, so the sampler would keep standing down on it")
	}

	// Every engine timing out is the same case, and was already handled.
	s.publishRaceAgg(raceAgg{}, 4, 0)
	if s.fleet.race.torrents != 100 {
		t.Fatalf("a fully failed poll blanked the overview: %+v", s.fleet.race)
	}

	// No agents at all is not a failure: an empty fleet really is empty.
	s.publishRaceAgg(raceAgg{}, 0, 0)
	if s.fleet.race.torrents != 0 {
		t.Fatalf("with no engines left the stale totals stayed: %+v", s.fleet.race)
	}
}

// The peers slice belongs to the caller.
func TestRacePeersJSONDoesNotReorderTheCallersSlice(t *testing.T) {
	peers := []ltclient.PeerInfo{
		{IP: "10.0.0.1", DLRate: 1},
		{IP: "10.0.0.2", DLRate: 500},
	}
	_ = racePeersJSON(peers)
	if peers[0].IP != "10.0.0.1" {
		t.Fatal("racePeersJSON sorted the caller's slice in place")
	}
}
