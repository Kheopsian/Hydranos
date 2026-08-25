package config

import "testing"

func cfgWithAgents(ags ...AgentConfig) *HydraConfig {
	c := &HydraConfig{}
	c.Race.ListenPort = 16171
	c.Hoard.ListenPort = 16172
	c.Agents = ags
	return c
}

// An [[agent]] without addr runs here: it becomes an engine of this process.
// This is the shape the config is converging on now that one agent means one
// engine.
func TestAgentWithoutAddrBecomesALocalEngine(t *testing.T) {
	c := cfgWithAgents(AgentConfig{Name: "local-vpn7", Role: "race",
		Session: SessionConfig{ListenPort: 26991}})
	engines, err := c.ResolveEngines()
	if err != nil {
		t.Fatal(err)
	}
	var found *EngineConfig
	for i := range engines {
		if engines[i].ID == "local-vpn7" {
			found = &engines[i]
		}
	}
	if found == nil {
		t.Fatal("a local [[agent]] entry did not become an engine")
	}
	if found.Role != "race" || found.ListenPort != 26991 {
		t.Errorf("engine = %+v, want race/26991", found)
	}
	// Additive, not exclusive: the two primaries must survive. [[engine]]
	// blocks replace them silently and that is the trap this array replaces.
	if len(engines) != 3 {
		t.Errorf("%d engines, want 3: the primaries were displaced", len(engines))
	}
}

// An entry WITH an addr is another machine. Starting it here would turn a
// remote node into a local one, running someone else's engine on this host.
func TestAgentWithAddrIsNotStartedHere(t *testing.T) {
	c := cfgWithAgents(AgentConfig{Name: "seedbox", Addr: "10.0.0.5:9090", Role: "race"})
	engines, err := c.ResolveEngines()
	if err != nil {
		t.Fatal(err)
	}
	for _, e := range engines {
		if e.ID == "seedbox" {
			t.Fatal("a remote agent was started as a local engine")
		}
	}
}

// A role is required: an entry that merely forgot its addr must not be silently
// started here.
func TestAgentWithoutRoleIsIgnored(t *testing.T) {
	c := cfgWithAgents(AgentConfig{Name: "half-written"})
	engines, _ := c.ResolveEngines()
	for _, e := range engines {
		if e.ID == "half-written" {
			t.Fatal("an entry with no role was started as an engine")
		}
	}
}

// Reusing a primary's id overrides it rather than colliding: a node can change
// its own race engine without restating the rest of the config.
func TestLocalAgentCanOverrideAPrimary(t *testing.T) {
	c := cfgWithAgents(AgentConfig{Name: "race", Role: "race",
		Session: SessionConfig{ListenPort: 20000, BindInterface: "wg9"}})
	engines, err := c.ResolveEngines()
	if err != nil {
		t.Fatalf("override reported a duplicate instead of replacing: %v", err)
	}
	if len(engines) != 2 {
		t.Fatalf("%d engines, want 2", len(engines))
	}
	for _, e := range engines {
		if e.ID == "race" && (e.ListenPort != 20000 || e.BindInterface != "wg9") {
			t.Errorf("primary not overridden: %+v", e)
		}
	}
}
