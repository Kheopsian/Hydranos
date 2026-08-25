package api

import "testing"

// TestTheBareLocalNameKeepsWorking is the compatibility guard. "local" is
// written into existing categories, save-path overrides and job params. If it
// ever stops meaning "this node", every one of those configs starts routing a
// local add, pause or bulk action at an agent that was never dialled -- and the
// user changed nothing.
func TestTheBareLocalNameKeepsWorking(t *testing.T) {
	for _, n := range []string{LocalAgentName, LocalAgentRace, LocalAgentHoard} {
		if !isLocalAgentName(n) {
			t.Errorf("%q is not recognised as this node: local dispatch would go looking for a remote agent", n)
		}
	}
	for _, n := range []string{"", "hydra2", "localhost", "local2", "notlocal"} {
		if isLocalAgentName(n) {
			t.Errorf("%q wrongly claims to be this node: actions meant for it would run here, on the wrong machine", n)
		}
	}
}

// Only the per-role names pin an engine. The bare "local" must NOT, or every
// existing category suddenly forces a role its mode never asked for.
func TestOnlyPerRoleNamesPinAnEngine(t *testing.T) {
	if _, pinned := roleOfLocalAgent(LocalAgentName); pinned {
		t.Error(`the bare "local" pinned a role; existing categories would be forced onto one engine`)
	}
	for name, want := range map[string]string{LocalAgentRace: "race", LocalAgentHoard: "hoard"} {
		got, pinned := roleOfLocalAgent(name)
		if !pinned || got != want {
			t.Errorf("%q pinned %q/%v, want %q/true", name, got, pinned, want)
		}
	}
}

// A dialled agent must not be allowed to take a name this node answers to:
// every action meant for it would be executed here instead, silently and on the
// wrong machine.
func TestReservedNamesCannotBeTakenByARemoteAgent(t *testing.T) {
	for _, n := range []string{"local", "local-race", "local-hoard", "  local-race  "} {
		if !isReservedAgentName(n) {
			t.Errorf("%q accepted as a remote agent name", n)
		}
	}
	if isReservedAgentName("hydra2") {
		t.Error("a legitimate agent name was refused")
	}
}

// The role -> name mapping must round-trip, or registration and lookup disagree
// and the node registers engines nobody can address.
func TestRoleAndNameRoundTrip(t *testing.T) {
	for _, role := range []string{"race", "hoard"} {
		name := localAgentForRole(role)
		got, pinned := roleOfLocalAgent(name)
		if !pinned || got != role {
			t.Errorf("role %q -> name %q -> role %q (pinned=%v)", role, name, got, pinned)
		}
	}
}
