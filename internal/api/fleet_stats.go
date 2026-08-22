package api

import (
	"sync"
	"time"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
	"github.com/gin-gonic/gin"
)

// Fleet totals for the overview.
//
// statusPayload reads the local engines and nothing else. On a controller
// those are the empty stubs, so every card on the home page sat at zero and
// never moved while the agents were seeding at full rate. The numbers there
// are fleet numbers -- torrents, peers, rates -- not this node's numbers, so
// the agents belong in them wherever the engines happen to run.
//
// Race is sampled on its own short loop, browser or not, so /api/status stays
// true for a monitor: the listing is a handful of torrents. Hoard rides the
// agent-row poll instead (see agentrows.go) -- re-listing a six-figure agent a
// second time just to total it up is the one thing that poll exists to avoid.
const agentRaceStatsInterval = 2 * time.Second

type raceAgg struct {
	torrents    int
	withPeers   int
	peers       int
	activeDL    int
	activeSeeds int
	ulRate      int64
	dlRate      int64
}

// fleetStats is the last good sample of what the agents' race engines are
// doing. A failed poll leaves the previous one standing rather than blanking
// the page for a tick.
type fleetStats struct {
	mu   sync.RWMutex
	race raceAgg
}

func (s *Server) startAgentRaceStatsSampler() {
	go func() {
		t := time.NewTicker(agentRaceStatsInterval)
		defer t.Stop()
		for range t.C {
			s.refreshAgentRaceStats()
		}
	}()
}

func (s *Server) refreshAgentRaceStats() {
	var agg raceAgg
	engines, answered := 0, 0
	for _, ra := range s.agentsSnapshot() {
		for _, e := range ra.byRole("race") {
			if e.client == nil {
				continue
			}
			engines++
			lst, err := e.client.ListTorrentsTimeout(4 * time.Second)
			if err != nil || lst == nil {
				continue
			}
			answered++
			for _, t := range lst.Torrents {
				agg.add(t)
			}
		}
	}
	// Every engine timed out at once: keep the last good sample. Publishing
	// zeros here would read as "the fleet stopped", which is the opposite of
	// what an unreachable agent means.
	if engines > 0 && answered == 0 {
		return
	}
	s.fleet.mu.Lock()
	s.fleet.race = agg
	s.fleet.mu.Unlock()
}

func (a *raceAgg) add(t ltclient.TorrentStatus) {
	a.torrents++
	a.peers += t.NumPeers
	a.ulRate += int64(t.UploadRate)
	a.dlRate += int64(t.DownloadRate)
	if t.NumPeers > 0 {
		a.withPeers++
	}
	switch t.State {
	case "downloading":
		a.activeDL++
	case "seeding":
		a.activeSeeds++
	}
}

// addAgentRaceStats folds the agents' race engines into a status payload built
// from the local one. Addition, not replacement: a monolith that also drives
// agents is running both, and the overview is the one place that shows all of
// it. The hoard half goes through mergeAgentHoardStats, which the hoard header
// already uses, so both headers total the same rows the same way.
func (s *Server) addAgentRaceStats(result gin.H) {
	s.fleet.mu.RLock()
	race := s.fleet.race
	s.fleet.mu.RUnlock()
	if race.torrents == 0 {
		return
	}
	m := statsMap(result, "race")
	addNum(m, "torrents", race.torrents)
	addNum(m, "total_peers", race.peers)
	addNum(m, "torrents_with_peers", race.withPeers)
	addNum(m, "active_downloads", race.activeDL)
	addNum(m, "active_seeds", race.activeSeeds)
	addNum(m, "total_upload_rate", race.ulRate)
	addNum(m, "total_download_rate", race.dlRate)
}

// statsMap returns the payload's sub-map for an engine, creating it when the
// local engine contributed nothing -- which is every payload on a controller.
func statsMap(result gin.H, key string) map[string]interface{} {
	if m, ok := result[key].(map[string]interface{}); ok && m != nil {
		return m
	}
	m := map[string]interface{}{}
	result[key] = m
	return m
}

// addNum adds to whatever numeric type the engine put there, keeping int64 for
// the byte counters and int for the counts.
func addNum(m map[string]interface{}, key string, delta interface{}) {
	switch d := delta.(type) {
	case int:
		m[key] = toInt(m[key]) + d
	case int64:
		m[key] = toInt64(m[key]) + d
	}
}

func toInt(v interface{}) int {
	switch n := v.(type) {
	case int:
		return n
	case int64:
		return int(n)
	case float64:
		return int(n)
	}
	return 0
}

func toInt64(v interface{}) int64 {
	switch n := v.(type) {
	case int:
		return int64(n)
	case int64:
		return n
	case float64:
		return int64(n)
	}
	return 0
}
