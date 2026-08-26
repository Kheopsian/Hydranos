package api

import "testing"

// The list rows carry their real agent name since 3.146.0 -- local-race,
// local-hoard, local-<id> -- and the UI sends it back on every per-row request.
// Every "is this mine" test has to accept those names: read as a remote agent,
// a detail lookup for this node's own torrent goes over the agent wire (or, for
// a name nobody dialled, answers "torrent not found" about a torrent that is
// right here).
func TestThisNodesOwnNamesAreNotTreatedAsRemote(t *testing.T) {
	s := &Server{}
	s.agentRows.byAgent = map[string]rowSet{
		"seedbox": {"aa": {"info_hash": "aa", "agent_engine": "hoard-0"}},
	}
	s.remoteAgents = []*remoteAgent{
		{name: "seedbox", engines: []remoteEngine{{id: "hoard-0"}}},
		{name: LocalAgentHoard, local: true, engines: []remoteEngine{{id: "hoard"}}},
	}

	for _, name := range []string{"local", LocalAgentHoard, LocalAgentRace, "local-vpn7"} {
		if _, _, _, ok := s.resolveHoardDetailTarget("aa", name); ok {
			t.Errorf("hint %q resolved to a remote target: this node's own row would be fetched over the wire", name)
		}
	}
	// A genuinely remote name still resolves, or the fix would have cut the
	// agents off instead.
	if _, _, _, ok := s.resolveHoardDetailTarget("aa", "seedbox"); !ok {
		t.Error("a remote agent hint stopped resolving")
	}
}

// isLocalAgentName is the rule the UI mirrors in _isLocalAgent. If they drift,
// the browser and the daemon disagree about which rows are this node's: live
// stats skipped on one side, actions dialled at nobody on the other.
func TestEveryNameThisNodeAnswersTo(t *testing.T) {
	for _, name := range []string{"local", "local-race", "local-hoard", "local-vpn7", "local-anything"} {
		if !isLocalAgentName(name) {
			t.Errorf("%q is not recognised as this node", name)
		}
	}
	for _, name := range []string{"seedbox", "localhost", "localbox", ""} {
		if name != "" && isLocalAgentName(name) {
			t.Errorf("%q was taken for this node", name)
		}
	}
}
