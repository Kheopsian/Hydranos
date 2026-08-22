package api

import (
	"strings"
	"testing"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/config"
)

func TestAgentConfigError(t *testing.T) {
	// A complete block registers; the optional fields stay optional.
	if got := agentConfigError(config.AgentConfig{Name: "boreas", Addr: "10.0.0.2:9090"}); got != "" {
		t.Fatalf("a usable [[agent]] block was rejected: %q", got)
	}
	// Each missing field is named, so the log line is enough to fix the TOML
	// without opening the source.
	cases := []struct {
		ag   config.AgentConfig
		want string
	}{
		{config.AgentConfig{Addr: "10.0.0.2:9090"}, "name"},
		{config.AgentConfig{Name: "boreas"}, "addr"},
		{config.AgentConfig{}, "both"},
	}
	for _, c := range cases {
		got := agentConfigError(c.ag)
		if got == "" {
			t.Fatalf("an unusable block %+v was accepted", c.ag)
		}
		if !strings.Contains(got, c.want) {
			t.Fatalf("the reason for %+v does not name %q: %q", c.ag, c.want, got)
		}
	}
}

// composeTestConfig is a front holding one fleet profile per role plus a
// per-engine exception, which is the whole shape the composer has to get right.
func composeTestConfig() *config.HydraConfig {
	cfg := config.DefaultConfig()
	cfg.Race.MaxConnections = 4000
	cfg.Race.AnnounceProxy = "socks5h://fleet:1080"
	cfg.Hoard.MaxConnections = 8000
	cfg.Agents = []config.AgentConfig{{
		Name: "de-1", Addr: "10.0.0.5:9090",
		EngineOverrides: []map[string]interface{}{
			{"id": "race-0", "announce_proxy": "socks5h://de:1080"},
		},
	}}
	return cfg
}

func TestComposeSessionAppliesProfileThenOverride(t *testing.T) {
	cfg := composeTestConfig()

	// An engine with no override block gets the fleet profile verbatim.
	hoard, err := cfg.ComposeSession("de-1", "hoard-0", "hoard")
	if err != nil {
		t.Fatalf("compose hoard: %v", err)
	}
	if hoard.MaxConnections != 8000 {
		t.Fatalf("the hoard profile did not reach the payload: max_connections = %d, want 8000", hoard.MaxConnections)
	}

	// An override replaces only the key it names; the rest of the profile
	// stays, which is the point of writing overrides sparsely.
	race, err := cfg.ComposeSession("de-1", "race-0", "race")
	if err != nil {
		t.Fatalf("compose race: %v", err)
	}
	if race.AnnounceProxy != "socks5h://de:1080" {
		t.Fatalf("the per-engine override was not applied: announce_proxy = %q", race.AnnounceProxy)
	}
	if race.MaxConnections != 4000 {
		t.Fatalf("the override wiped the rest of the profile: max_connections = %d, want 4000", race.MaxConnections)
	}
	if race.CustomChoking == nil || !race.CustomChoking.Enabled {
		t.Fatal("the profile's [race.custom_choking] sub-table was lost in the merge")
	}
}

// The listen port is a fact about the agent's machine, so the front must not
// be able to state one: an agent overlays its own, and shipping a value that
// is silently ignored is how a config comes to read differently from reality.
func TestComposeSessionOmitsAgentOwnedIdentity(t *testing.T) {
	cfg := composeTestConfig()
	cfg.Race.ListenPort = 16171
	cfg.Race.EnableIPv6 = true

	race, err := cfg.ComposeSession("de-1", "race-0", "race")
	if err != nil {
		t.Fatalf("compose: %v", err)
	}
	if race.ListenPort != 0 || race.EnableIPv6 {
		t.Fatalf("the front tried to dictate the agent's own identity: listen_port=%d enable_ipv6=%v",
			race.ListenPort, race.EnableIPv6)
	}
}

// The revision is a content hash so that re-pushing an unchanged config is a
// no-op the agent can recognise. It therefore has to be stable across calls
// and across the order list_engines happened to answer in.
func TestComposedRevisionIsStableAndOrderIndependent(t *testing.T) {
	s := &Server{config: composeTestConfig()}
	cfg := s.config
	forward := []agentwire.EngineDescriptor{{ID: "race-0", Role: "race"}, {ID: "hoard-0", Role: "hoard"}}
	backward := []agentwire.EngineDescriptor{{ID: "hoard-0", Role: "hoard"}, {ID: "race-0", Role: "race"}}

	a, err := s.composeAgentConfig(cfg, "de-1", forward)
	if err != nil {
		t.Fatalf("compose: %v", err)
	}
	b, err := s.composeAgentConfig(cfg, "de-1", backward)
	if err != nil {
		t.Fatalf("compose: %v", err)
	}
	if a.Revision == 0 {
		t.Fatal("revision is zero: every push would look like a change")
	}
	if a.Revision != b.Revision {
		t.Fatalf("the revision moved with the engine order: %d vs %d", a.Revision, b.Revision)
	}

	// And it does move when the configuration actually changes, otherwise an
	// agent would never be told to re-apply.
	cfg.Race.MaxConnections = 1234
	c, err := s.composeAgentConfig(cfg, "de-1", forward)
	if err != nil {
		t.Fatalf("compose: %v", err)
	}
	if c.Revision == a.Revision {
		t.Fatal("the revision ignored a real config change")
	}
}

func TestComposeSessionRejectsUnknownRole(t *testing.T) {
	cfg := composeTestConfig()
	if _, err := cfg.ComposeSession("de-1", "weird-0", "seedbox"); err == nil {
		t.Fatal("an engine with an unknown role was composed for instead of reported")
	}
}
