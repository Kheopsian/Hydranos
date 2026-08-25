package api

import "testing"

// TestShardNamesAreRecognisedAsLocal is what dissolving "local-shards" rests
// on. Extra engines carry operator-chosen ids, so their agent names cannot be
// enumerated in advance -- recognition is by prefix. Miss it and a local add,
// pause or bulk action for a shard goes looking for an agent that was never
// dialled.
func TestShardNamesAreRecognisedAsLocal(t *testing.T) {
	for _, id := range []string{"race", "hoard", "race-2", "seedbox-fr", "vpn7"} {
		name := LocalAgentNameFor(id)
		if !isLocalAgentName(name) {
			t.Errorf("engine %q -> agent %q is not recognised as this node", id, name)
		}
		if !isReservedAgentName(name) {
			t.Errorf("agent %q can be claimed by a dialled node: its actions would run here instead", name)
		}
	}
	// The two primaries keep the exact names the rest of the code knows.
	if LocalAgentNameFor("race") != LocalAgentRace || LocalAgentNameFor("hoard") != LocalAgentHoard {
		t.Error("the primaries' agent names drifted from the constants")
	}
}

// A shard's role comes from the registry, not from its name. Falling back to
// the category's mode would put a race torrent on the hoard engine of the right
// node -- right machine, wrong tunnel, which is the whole point of naming it.
func TestShardRoleComesFromTheRegistry(t *testing.T) {
	s := &Server{}
	cl := newLocalAgentClient("vpn7", &fakeEngine{}, &countingAgent{})
	if err := s.AddLocalAgent(LocalAgentNameFor("vpn7"), "vpn7", "race", cl); err != nil {
		t.Fatal(err)
	}
	role, pinned := s.roleOfLocalAgentIn("local-vpn7")
	if !pinned || role != "race" {
		t.Errorf("role = %q pinned=%v, want race/true", role, pinned)
	}
	// The bare alias still pins nothing, so existing categories keep their mode.
	if _, pinned := s.roleOfLocalAgentIn(LocalAgentName); pinned {
		t.Error(`the bare "local" pinned a role`)
	}
	// An unknown local name pins nothing rather than guessing.
	if _, pinned := s.roleOfLocalAgentIn("local-nope"); pinned {
		t.Error("an unregistered local name pinned a role out of thin air")
	}
}
