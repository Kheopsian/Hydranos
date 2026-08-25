package api

import "testing"

// TestAddLocalAgentAccumulatesEnginesUnderOneName guards the difference between
// the local and the remote registration path. The remote one learns an agent's
// whole engine list in a single ListEngines and therefore REPLACES; the local
// one is called once per engine, so replacing would mean registering the hoard
// engine silently dropped the race engine registered a moment earlier -- one
// half of the node quietly missing from placement, with nothing failing.
func TestAddLocalAgentAccumulatesEnginesUnderOneName(t *testing.T) {
	s := &Server{}
	race := newLocalAgentClient("local-race", &fakeEngine{}, &countingAgent{})
	hoard := newLocalAgentClient("local-hoard", &fakeEngine{}, &countingAgent{})

	if err := s.AddLocalAgent("local", "local-race", "race", race); err != nil {
		t.Fatal(err)
	}
	if err := s.AddLocalAgent("local", "local-hoard", "hoard", hoard); err != nil {
		t.Fatal(err)
	}

	ra := s.remoteAgentByName("local")
	if ra == nil {
		t.Fatal("no agent registered")
	}
	if len(ra.engines) != 2 {
		t.Fatalf("agent hosts %d engine(s), want 2: the second registration replaced the first", len(ra.engines))
	}
	// Both must stay addressable, by id and by role.
	for _, sel := range []string{"local-race", "local-hoard", "race", "hoard"} {
		if cl, _ := ra.resolveEngine(sel); cl == nil {
			t.Errorf("selector %q resolves to nothing", sel)
		}
	}
}

// Re-registering the same id must swap the client, not add a duplicate: an
// engine that restarts would otherwise accumulate stale entries, and
// resolveEngine returns the first match -- the dead one.
func TestAddLocalAgentReplacesTheSameEngineID(t *testing.T) {
	s := &Server{}
	first := newLocalAgentClient("local-race", &fakeEngine{}, &countingAgent{})
	second := newLocalAgentClient("local-race", &fakeEngine{}, &countingAgent{})
	_ = s.AddLocalAgent("local", "local-race", "race", first)
	_ = s.AddLocalAgent("local", "local-race", "race", second)

	ra := s.remoteAgentByName("local")
	if len(ra.engines) != 1 {
		t.Fatalf("%d entries for one engine id: resolveEngine would return the stale one", len(ra.engines))
	}
	if cl, _ := ra.resolveEngine("local-race"); cl != second {
		t.Error("the stale client survived the re-registration")
	}
}

// A caller that forgets an argument must be told, not silently registered as an
// agent that resolves to a nil client and panics on first use.
func TestAddLocalAgentRefusesIncompleteInput(t *testing.T) {
	s := &Server{}
	cl := newLocalAgentClient("x", &fakeEngine{}, &countingAgent{})
	for _, tc := range []struct {
		name, id string
		c        AgentClient
	}{
		{"", "local-race", cl},
		{"local", "", cl},
		{"local", "local-race", nil},
	} {
		if err := s.AddLocalAgent(tc.name, tc.id, "race", tc.c); err == nil {
			t.Errorf("accepted name=%q id=%q client=%v", tc.name, tc.id, tc.c != nil)
		}
	}
	if len(s.remoteAgents) != 0 {
		t.Errorf("a refused registration still left %d agent(s) behind", len(s.remoteAgents))
	}
}
