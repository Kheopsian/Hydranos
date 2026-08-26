package api

import "testing"

// The exclusion exists for exactly two agents: the ones the local path already
// reports. An extra engine is reported by nothing else, so leaving it out made
// its torrents invisible everywhere -- and a torrent handed to it looked lost.
func TestExtraLocalEnginesAreCollectedLikeAgents(t *testing.T) {
	s := &Server{remoteAgents: []*remoteAgent{
		{name: LocalAgentRace, local: true},
		{name: LocalAgentHoard, local: true},
		{name: "local-vpn7", local: true},
		{name: "seedbox"},
	}}
	got := map[string]bool{}
	for _, ra := range s.agentsSnapshot() {
		got[ra.name] = true
	}
	if !got["local-vpn7"] {
		t.Error("an extra engine of this node is collected by nobody: its torrents are invisible")
	}
	if !got["seedbox"] {
		t.Error("a remote agent stopped being collected")
	}
	// And the two the local path already contributes stay out, or every
	// aggregate counts them twice.
	if got[LocalAgentRace] || got[LocalAgentHoard] {
		t.Error("a primary engine is collected twice: once directly, once as an agent")
	}
	if n := len(s.allAgentsSnapshot()); n != 4 {
		t.Errorf("allAgentsSnapshot = %d, want all 4", n)
	}
}
