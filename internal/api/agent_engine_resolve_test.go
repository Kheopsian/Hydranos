package api

import "testing"

// A remote agent names its engines from its own config ("race-0"), while the
// control plane addresses them by role ("race"). Resolution must bridge both,
// or a routed add answers `engine "race" not wired on agent`.
func TestResolveEngineMapsRoleToConfigID(t *testing.T) {
	ra := &remoteAgent{name: "n1", engines: []remoteEngine{
		{id: "race-0", role: "race"},
		{id: "hoard-cold", role: "hoard"},
	}}
	for _, tc := range []struct{ sel, want string }{
		{"race", "race-0"},
		{"hoard", "hoard-cold"},
		{"race-0", "race-0"},
		{"hoard-cold", "hoard-cold"},
	} {
		if _, got := ra.resolveEngine(tc.sel); got != tc.want {
			t.Errorf("resolveEngine(%q) = %q, want %q", tc.sel, got, tc.want)
		}
	}
}

func TestResolveEnginePrefersIDOverRole(t *testing.T) {
	// An id that collides with another engine's role must not be hijacked.
	ra := &remoteAgent{engines: []remoteEngine{
		{id: "hoard", role: "race"},
		{id: "h2", role: "hoard"},
	}}
	if _, got := ra.resolveEngine("hoard"); got != "hoard" {
		t.Errorf("resolveEngine(\"hoard\") = %q, want the id match \"hoard\"", got)
	}
}

func TestResolveEngineUnknownIsNotWired(t *testing.T) {
	ra := &remoteAgent{engines: []remoteEngine{{id: "hoard-0", role: "hoard"}}}
	if cl, _ := ra.resolveEngine("race"); cl != nil {
		t.Error("resolveEngine returned a client for a role the agent does not host")
	}
}
