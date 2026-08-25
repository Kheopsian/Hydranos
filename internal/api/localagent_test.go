package api

import (
	"testing"
	"time"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// countingAgent records every call that reaches the agent wire.
type countingAgent struct {
	AgentClient
	calls int
}

func (c *countingAgent) NodeInfo() (agentwire.NodeInfo, error) {
	c.calls++
	return agentwire.NodeInfo{}, nil
}
func (c *countingAgent) DiskFree(string) (int64, error) { c.calls++; return 0, nil }

// TorrentCategories is stubbed because the row collector calls it for every
// agent it lists. Left to the nil embedded interface it panics, which says
// nothing about the code under test.
func (c *countingAgent) TorrentCategories(string) (map[string]string, error) {
	c.calls++
	return map[string]string{}, nil
}

// fakeEngine answers the hot path and counts too, so a test cannot pass by
// nobody being called at all.
type fakeEngine struct {
	engine.EngineClient // nil on purpose: any method this test does not stub would panic
	calls               int
}

func (f *fakeEngine) ListTorrents() (*ltclient.ListTorrentsResult, error) {
	f.calls++
	return &ltclient.ListTorrentsResult{Count: 7}, nil
}
func (f *fakeEngine) GetSessionStats() (*ltclient.SessionStats, error) {
	f.calls++
	return &ltclient.SessionStats{}, nil
}
func (f *fakeEngine) GetStatus(string) (*ltclient.TorrentStatus, error) {
	f.calls++
	return &ltclient.TorrentStatus{}, nil
}

// TestLocalAgentKeepsTheHotPathOffTheWire is the guard on the one number that
// justifies localAgentClient existing at all. At 198k torrents a list through
// the agent wire costs 1.68 s and 954 MB against 376 ns and no allocation in
// process, because the wire JSON-encodes every reply. The tempting cleanup --
// "just delegate everything to l.agent, it is simpler" -- would reintroduce
// that silently: correct results, same API, an order of magnitude more garbage,
// and nothing failing to show it.
//
// Proved by breaking it: point any of these three at l.agent and this goes red.
func TestLocalAgentKeepsTheHotPathOffTheWire(t *testing.T) {
	ag := &countingAgent{}
	eng := &fakeEngine{}
	l := newLocalAgentClient("race", eng, ag)

	if _, err := l.ListTorrents(); err != nil {
		t.Fatal(err)
	}
	if _, err := l.ListTorrentsTimeout(time.Second); err != nil {
		t.Fatal(err)
	}
	if _, err := l.GetSessionStats(); err != nil {
		t.Fatal(err)
	}
	if _, err := l.GetStatus("abc"); err != nil {
		t.Fatal(err)
	}

	if ag.calls != 0 {
		t.Errorf("%d hot-path call(s) went through the agent wire, want 0: at production scale that is ~1 GB of garbage per list", ag.calls)
	}
	if eng.calls != 4 {
		t.Errorf("engine saw %d calls, want 4: the hot path is not reaching the engine at all, so the assertion above proves nothing", eng.calls)
	}
}

// The cold path must go the OTHER way: through the agent, so a local node
// answers node-level questions exactly as a remote one does instead of growing
// its own slightly different implementation.
func TestLocalAgentSendsColdCallsThroughTheAgent(t *testing.T) {
	ag := &countingAgent{}
	l := newLocalAgentClient("race", &fakeEngine{}, ag)
	if _, err := l.NodeInfo(); err != nil {
		t.Fatal(err)
	}
	if _, err := l.DiskFree("/data"); err != nil {
		t.Fatal(err)
	}
	if ag.calls != 2 {
		t.Errorf("agent saw %d cold calls, want 2", ag.calls)
	}
}

// An empty engine selector must resolve to THIS client's engine. A remote
// client is built per engine, so callers legitimately pass ""; forwarding the
// blank would let the agent answer from whichever engine it listed first, and a
// category read for the race engine could come back with the hoard's.
func TestLocalAgentDefaultsTheEngineSelectorToItsOwn(t *testing.T) {
	l := newLocalAgentClient("race", &fakeEngine{}, &countingAgent{})
	if got := l.engineOr(""); got != "race" {
		t.Errorf("empty selector resolved to %q, want %q", got, "race")
	}
	if got := l.engineOr("hoard"); got != "hoard" {
		t.Errorf("explicit selector rewritten to %q", got)
	}
}
