package api

import "testing"

// TestLocalAgentIsListedLikeAnyOther guards the swap from a synthesised entry
// to a registered one. The synthesised version reported an engine "online" from
// a non-nil pointer alone -- it never pinged anything -- so a wedged local
// engine showed green while every action against it hung. Going through the
// same loop as a remote agent means online now means answered.
func TestLocalAgentIsListedLikeAnyOther(t *testing.T) {
	s := &Server{}
	cl := newLocalAgentClient("race", &fakeEngine{}, &countingAgent{})
	if err := s.AddLocalAgent(LocalAgentName, "race", "race", cl); err != nil {
		t.Fatal(err)
	}

	// allAgentsSnapshot, not agentsSnapshot: the latter deliberately hides this
	// node so the dozen aggregating callers cannot count it twice.
	snap := s.allAgentsSnapshot()
	if len(s.agentsSnapshot()) != 0 {
		t.Error("the local agent leaked into the aggregating snapshot; every total that adds agents on top of the local counters would double")
	}
	if len(snap) != 1 {
		t.Fatalf("%d agent(s) in the snapshot, want 1", len(snap))
	}
	if !snap[0].local {
		t.Error("the local agent is not flagged local, so the UI would show it as a remote node with no address")
	}
	if snap[0].addr != "" {
		t.Errorf("addr = %q: a local node has no address, and showing one advertises a port nothing listens on", snap[0].addr)
	}
	if cl, _ := snap[0].resolveEngine("race"); cl == nil {
		t.Error("the local engine is not resolvable through the agent path")
	}
}

// A remote agent must NOT be flagged local: the flag decides what the UI is
// told, and mislabelling a remote node hides its address from the operator.
func TestRemoteAgentIsNotFlaggedLocal(t *testing.T) {
	s := &Server{}
	s.remoteAgents = append(s.remoteAgents, &remoteAgent{name: "hydra2", addr: "10.0.0.2:9099"})
	if s.allAgentsSnapshot()[0].local {
		t.Error("a dialled agent came back flagged local")
	}
}
