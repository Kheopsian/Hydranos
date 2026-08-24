package api

import (
	"testing"
	"time"
)

// seedMetric plants a measurement so the selection logic can be tested without
// a live agent: agentMetric serves the cache first.
func seedMetric(agent, path string, free, rate int64, ok bool) {
	placementMetrics.put(agent+"\x00"+path, placementMetric{freeBytes: free, rate: rate, ok: ok})
}

func clearMetrics() {
	placementMetrics.mu.Lock()
	placementMetrics.m = map[string]placementMetric{}
	placementMetrics.mu.Unlock()
}

// The reserve drops a full agent from the candidates -- including under "all",
// where the fan-out has to skip it rather than keep filling a dying disk.
func TestEligibleTargetsHonoursReserve(t *testing.T) {
	clearMetrics()
	cat := &category{Name: "movies", SavePath: "/data/movies", MinFreeBytes: 100 << 30}
	seedMetric("local", "/data/movies", 500<<30, 0, true)
	seedMetric("full", "/data/movies", 10<<30, 0, true)

	got := (&Server{}).eligibleTargets(cat, []string{"local", "full"})
	if len(got) != 1 || got[0] != "local" {
		t.Fatalf("targets = %v, want [local]", got)
	}
}

// An agent we could not measure stays a candidate: a failed RPC is not proof
// that a disk is full, and refusing to place on every unreachable node would
// turn a metrics hiccup into a refused add.
func TestEligibleTargetsKeepsUnmeasurable(t *testing.T) {
	clearMetrics()
	cat := &category{Name: "movies", SavePath: "/data/movies", MinFreeBytes: 100 << 30}
	seedMetric("silent", "/data/movies", 0, 0, false)

	got := (&Server{}).eligibleTargets(cat, []string{"silent"})
	if len(got) != 1 {
		t.Fatalf("targets = %v, want the unmeasurable agent kept", got)
	}
}

// No reserve set = no filtering at all, and no metric calls.
func TestEligibleTargetsNoReserveIsPassthrough(t *testing.T) {
	clearMetrics()
	cat := &category{Name: "movies", SavePath: "/data/movies"}
	got := (&Server{}).eligibleTargets(cat, []string{"a", "b", "c"})
	if len(got) != 3 {
		t.Fatalf("targets = %v, want all three", got)
	}
}

// most_free_space reads the CATEGORY'S path on each agent, which is the whole
// point: the same category can point at very different filesystems per agent.
func TestPickBestMostFreeUsesPerAgentPath(t *testing.T) {
	clearMetrics()
	cat := &category{
		Name:     "movies",
		SavePath: "/data/movies",
		Agents:   map[string]string{"big": "/tank/movies"},
	}
	seedMetric("local", "/data/movies", 200<<30, 0, true)
	seedMetric("big", "/tank/movies", 900<<30, 0, true)

	got := (&Server{}).pickBest(cat, []string{"local", "big"}, func(m placementMetric, _ string) int64 {
		if !m.ok {
			return 0
		}
		return -m.freeBytes
	})
	if got != "big" {
		t.Fatalf("picked %q, want big (900 GiB at its own path)", got)
	}
}

// least_load picks the quietest agent by bytes/s currently moving.
func TestPickBestLeastLoad(t *testing.T) {
	clearMetrics()
	cat := &category{Name: "movies", SavePath: "/data/movies"}
	seedMetric("busy", "/data/movies", 1<<40, 800e6, true)
	seedMetric("idle", "/data/movies", 1<<40, 2e6, true)

	got := (&Server{}).pickBest(cat, []string{"busy", "idle"}, func(m placementMetric, _ string) int64 { return m.rate })
	if got != "idle" {
		t.Fatalf("picked %q, want idle", got)
	}
}

// Ties keep the order written in the category: an operator listing agents in a
// deliberate order should not get a coin flip.
func TestPickBestTieKeepsWrittenOrder(t *testing.T) {
	clearMetrics()
	cat := &category{Name: "movies", SavePath: "/data/movies"}
	seedMetric("first", "/data/movies", 1<<40, 5, true)
	seedMetric("second", "/data/movies", 1<<40, 5, true)

	got := (&Server{}).pickBest(cat, []string{"first", "second"}, func(m placementMetric, _ string) int64 { return m.rate })
	if got != "first" {
		t.Fatalf("picked %q, want first on a tie", got)
	}
}

// A stale measurement is not reused: the cache exists to absorb an import
// burst, not to remember a disk that filled up ten minutes ago.
func TestPlacementCacheExpires(t *testing.T) {
	clearMetrics()
	placementMetrics.mu.Lock()
	placementMetrics.m["a\x00/p"] = placementMetric{freeBytes: 1, ok: true, at: time.Now().Add(-2 * placementMetricTTL)}
	placementMetrics.mu.Unlock()
	if _, ok := placementMetrics.get("a\x00/p"); ok {
		t.Fatal("expired metric was served from the cache")
	}
}
