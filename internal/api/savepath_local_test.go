package api

import "testing"

// TestLegacyLocalSavePathStillApplies is the regression guard for what 3.136.0
// broke. Per-agent save paths were written when this node was a single agent
// called "local". Splitting it into local-race and local-hoard made that key
// match nothing, so the override was silently ignored and the category fell
// back to its flat save_path -- torrents landing on the disk the operator had
// deliberately moved them off, with nothing logged and nothing failing.
func TestLegacyLocalSavePathStillApplies(t *testing.T) {
	cat := category{SavePath: "/data/flat", Agents: map[string]string{"local": "/data/local-override"}}
	for _, agent := range []string{LocalAgentName, LocalAgentRace, LocalAgentHoard} {
		if got := cat.SavePathFor(agent); got != "/data/local-override" {
			t.Errorf("SavePathFor(%q) = %q, want the legacy override", agent, got)
		}
	}
}

// An exact per-engine key must beat the legacy one, so an operator can give one
// engine its own disk while the other keeps the shared path.
func TestPerEngineSavePathBeatsTheLegacyKey(t *testing.T) {
	cat := category{SavePath: "/data/flat", Agents: map[string]string{
		"local":      "/data/legacy",
		"local-race": "/data/race-only",
	}}
	if got := cat.SavePathFor(LocalAgentRace); got != "/data/race-only" {
		t.Errorf("race engine got %q, want its own path", got)
	}
	if got := cat.SavePathFor(LocalAgentHoard); got != "/data/legacy" {
		t.Errorf("hoard engine got %q, want the legacy shared path", got)
	}
}

// A remote agent must NOT pick up the local override: its disks are not ours.
func TestRemoteAgentIgnoresTheLocalOverride(t *testing.T) {
	cat := category{SavePath: "/data/flat", Agents: map[string]string{"local": "/data/local-override"}}
	if got := cat.SavePathFor("seedbox"); got != "/data/flat" {
		t.Errorf("remote agent got %q, want the flat path: it would write to a path that exists on another machine", got)
	}
}
