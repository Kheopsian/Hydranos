package api

import (
	"testing"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// listingEngine returns real rows, so a collector that wrongly picks this
// engine up produces visible duplicates rather than an empty list.
type listingEngine struct {
	fakeEngine
	n int
}

func (l *listingEngine) ListTorrents() (*ltclient.ListTorrentsResult, error) {
	ts := make([]ltclient.TorrentStatus, l.n)
	for i := range ts {
		ts[i] = ltclient.TorrentStatus{InfoHash: string(rune('a' + i)), Name: "t"}
	}
	return &ltclient.ListTorrentsResult{Torrents: ts, Count: l.n}, nil
}

// TestLocalAgentRowsAreNotCollectedTwice is the regression guard for the bug
// 3.135.0 shipped and prod rolled back within minutes of seeing.
//
// The agent-row collector feeds totals that are ADDED ON TOP of the local
// counters. Making this node's engines a registered agent silently enrolled
// them here, so every local torrent was counted once directly and once as an
// agent: /api/status reported 396592 for the 198296 rows the database holds,
// and every listing doubled with it. Nothing was written -- info_hash is a
// primary key -- but every number a user reads was wrong.
//
// What makes this worth a test rather than a comment: the failure is invisible
// unless you compare counts before and after. The build was green, the tests
// were green, and the agents list itself was byte-identical.
func TestLocalAgentRowsAreNotCollectedTwice(t *testing.T) {
	s := &Server{}
	local := newLocalAgentClient("hoard", &listingEngine{n: 5}, &countingAgent{})
	if err := s.AddLocalAgent(LocalAgentName, "hoard", "hoard", local); err != nil {
		t.Fatal(err)
	}

	rows := s.forceRefreshAgentHoardRows()

	if got := len(rows); got != 0 {
		t.Errorf("collector returned %d row(s) for this node's own engine; those get added on top of the local counters, so every local torrent is counted twice", got)
	}
	if r := s.agentRowsFor(LocalAgentName); len(r) != 0 {
		t.Errorf("%d cached row(s) under %q: the listing doubles as well as the totals", len(r), LocalAgentName)
	}
}

// A genuinely remote agent must still be collected, or excluding the local one
// would have silently emptied the multi-node view instead.
func TestRemoteAgentRowsAreStillCollected(t *testing.T) {
	s := &Server{}
	s.remoteAgents = append(s.remoteAgents, &remoteAgent{
		name: "hydra2", addr: "10.0.0.2:9099",
		engines: []remoteEngine{{id: "hoard", role: "hoard",
			client: newLocalAgentClient("hoard", &listingEngine{n: 3}, &countingAgent{})}},
	})
	if got := len(s.forceRefreshAgentHoardRows()); got != 3 {
		t.Errorf("collector returned %d row(s) for a remote agent, want 3: the exclusion caught the wrong agents", got)
	}
}
