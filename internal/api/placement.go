package api

// Placement strategies: which agent(s) a new torrent in a category lands on.
//
// The old menu was "all" (fan-out to every agent, i.e. multi-home) or
// "least_torrents" -- and a torrent count is a poor proxy for anything that
// matters. Ten thousand small torrents and ten Blu-ray remuxes are the same
// number and nowhere near the same disk, so the "emptiest" agent by that
// metric is regularly the fullest one in bytes.
//
// What actually decides where a torrent should go is the state of the
// CATEGORY'S OWN PATH on each candidate agent -- categories already carry a
// per-agent save path override, and two agents can point the same category at
// filesystems of very different sizes. So the metrics here are taken at
// cat.SavePathFor(agent), never at the agent as a whole.

import (
	"fmt"
	"sync"
	"time"

	"github.com/Kheopsian/hydra/internal/drain"
)

// Strategy names accepted in a category's `strategy` field.
const (
	strategyAll           = "all"             // fan-out to every placement agent (multi-home)
	strategyLeastTorrents = "least_torrents"  // fewest torrents
	strategyMostFree      = "most_free_space" // most free bytes at the category's path
	strategyLeastLoad     = "least_load"      // lowest current transfer rate
	strategyFillThenNext  = "fill_then_next"  // first agent still above its reserve
)

// placementMetricTTL caches the per-agent metrics for one burst of adds. An
// import storm (a *arr grabbing a season, autobrr on a busy tracker) submits
// adds back to back, and without this each one would fan an RPC out to every
// candidate agent to re-read numbers that cannot have moved meaningfully.
const placementMetricTTL = 5 * time.Second

// placementRPCTimeout bounds one metric call. A candidate we cannot measure is
// not a candidate: better to place on an agent we know about than to block an
// add behind a node that is wedged.
const placementRPCTimeout = 4 * time.Second

type placementMetric struct {
	freeBytes int64
	rate      int64 // upload+download bytes/s, the "is this node busy" signal
	ok        bool
	at        time.Time
}

type placementCache struct {
	mu sync.Mutex
	m  map[string]placementMetric // key: agent + "\x00" + path
}

var placementMetrics = &placementCache{m: map[string]placementMetric{}}

func (c *placementCache) get(key string) (placementMetric, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	v, ok := c.m[key]
	if !ok || time.Since(v.at) > placementMetricTTL {
		return placementMetric{}, false
	}
	return v, true
}

func (c *placementCache) put(key string, v placementMetric) {
	v.at = time.Now()
	c.mu.Lock()
	c.m[key] = v
	c.mu.Unlock()
}

// agentMetric reads free space and current load for one agent at the path this
// category uses THERE. Never fails an add: an unreachable agent comes back
// ok=false and is simply not selected.
func (s *Server) agentMetric(agent string, cat *category) placementMetric {
	path := cat.SavePathFor(agent)
	key := agent + "\x00" + path
	if v, ok := placementMetrics.get(key); ok {
		return v
	}
	m := placementMetric{}
	if agent == "local" {
		if _, free, err := drain.TotalFree(path); err == nil {
			m.freeBytes, m.ok = free, true
		}
		// AggregateStats folds the cached stats under a read lock; the list
		// forms of GetAllStatus build a map per torrent, which at 196k
		// torrents is not something an add should pay for.
		if s.raceEngine != nil {
			ag := s.raceEngine.AggregateStats()
			m.rate += toInt64(ag["total_upload_rate"]) + toInt64(ag["total_download_rate"])
		}
		if s.hoardEngine != nil {
			st := s.hoardEngine.GetAllStatus()
			m.rate += toInt64(st["active_upload_rate"]) + toInt64(st["active_download_rate"])
		}
	} else if ra := s.remoteAgentByName(agent); ra != nil {
		if cl := ra.anyClient(); cl != nil {
			if free, err := cl.DiskFree(path); err == nil {
				m.freeBytes, m.ok = free, true
			}
		}
		for _, e := range ra.engines {
			if e.client == nil {
				continue
			}
			if st, err := e.client.GetSessionStats(); err == nil && st != nil {
				m.rate += int64(st.UploadRate) + int64(st.DownloadRate)
			}
		}
	}
	placementMetrics.put(key, m)
	return m
}

// eligibleTargets drops candidates that sit below the category's reserve.
//
// The reserve applies to EVERY strategy, "all" included: a fan-out that keeps
// writing to a node with 8 GB left is how a disk reaches 100% and takes the
// whole engine down with it. An agent whose free space cannot be read is kept
// (we do not disqualify a node on a failed RPC), which is why this can only
// ever protect against a disk we can see filling up.
func (s *Server) eligibleTargets(cat *category, targets []string) []string {
	if cat.MinFreeBytes <= 0 {
		return targets
	}
	out := make([]string, 0, len(targets))
	for _, t := range targets {
		m := s.agentMetric(t, cat)
		if m.ok && m.freeBytes < cat.MinFreeBytes {
			continue
		}
		out = append(out, t)
	}
	return out
}

// pickBest returns the placement entry scoring lowest, ties going to the
// earlier entry so the order written in the category stays meaningful.
func (s *Server) pickBest(cat *category, targets []string, score func(placementMetric, string) int64) string {
	best, bestScore := "", int64(0)
	for _, t := range targets {
		sc := score(s.agentMetric(t, cat), t)
		if best == "" || sc < bestScore {
			best, bestScore = t, sc
		}
	}
	return best
}

// placementError explains a refusal in the terms the operator set it in.
type placementError struct {
	category string
	reserve  int64
}

func (e *placementError) Error() string {
	return fmt.Sprintf("category %q: every placement agent is below its %d-byte free-space reserve", e.category, e.reserve)
}
