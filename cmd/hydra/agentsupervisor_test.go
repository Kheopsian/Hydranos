package main

import (
	"context"
	"os"
	"path/filepath"
	"testing"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/config"
)

func testSupervisor(t *testing.T, boot ...agentBootEngine) *engineSupervisor {
	t.Helper()
	cfg := config.DefaultConfig()
	cfg.Daemon.DataDir = t.TempDir()
	return newEngineSupervisor(context.Background(), cfg, boot, nil)
}

// The listen port is a fact about the agent's machine. Whatever the front
// sends for it, the agent's own value wins: a front-side mistake must not be
// able to move a node's port, which takes a full announce cycle to undo on
// every tracker it seeds to.
func TestOverlayIdentityKeepsTheAgentsOwnPortAndFamily(t *testing.T) {
	sup := testSupervisor(t, agentBootEngine{ID: "race-0", Role: "race", ListenPort: 12314, EnableIPv6: true})

	got := sup.overlayIdentity("race-0", config.SessionConfig{
		ListenPort: 9999, EnableIPv6: false, MaxConnections: 4000,
	})
	if got.ListenPort != 12314 || !got.EnableIPv6 {
		t.Fatalf("the front overrode the agent's identity: listen_port=%d enable_ipv6=%v",
			got.ListenPort, got.EnableIPv6)
	}
	if got.MaxConnections != 4000 {
		t.Fatalf("the overlay dropped a pushed field: max_connections=%d", got.MaxConnections)
	}
}

func TestConfigCacheRoundTrip(t *testing.T) {
	sup := testSupervisor(t, agentBootEngine{ID: "race-0", Role: "race", ListenPort: 12314})
	want := agentwire.ApplyConfigParams{
		Revision: 42,
		Engines: []agentwire.AgentEngineConfig{
			{ID: "race-0", Role: "race", Session: config.SessionConfig{MaxConnections: 4000}},
		},
	}
	if err := sup.writeCache(want); err != nil {
		t.Fatalf("write cache: %v", err)
	}
	got, ok := sup.readCache()
	if !ok {
		t.Fatal("the cache just written was not read back")
	}
	if got.Revision != want.Revision || len(got.Engines) != 1 || got.Engines[0].Session.MaxConnections != 4000 {
		t.Fatalf("cache round-trip lost content: %+v", got)
	}
}

// A volume can be handed to a node with a different identity. Replaying the
// previous tenant's config would start engines this operator never declared.
func TestConfigCacheIsRejectedWhenItNamesOtherEngines(t *testing.T) {
	sup := testSupervisor(t, agentBootEngine{ID: "race-0", Role: "race"})
	stale := agentwire.ApplyConfigParams{
		Revision: 7,
		Engines:  []agentwire.AgentEngineConfig{{ID: "hoard-9", Role: "hoard"}},
	}
	if err := sup.writeCache(stale); err != nil {
		t.Fatalf("write cache: %v", err)
	}
	if _, ok := sup.readCache(); ok {
		t.Fatal("a cache belonging to another node's engines was accepted")
	}

	// Same id, different role is the same problem in a subtler form.
	sup2 := testSupervisor(t, agentBootEngine{ID: "race-0", Role: "hoard"})
	sup2.dataDir = sup.dataDir
	if _, ok := sup2.readCache(); ok {
		t.Fatal("a cache whose engine changed role was accepted")
	}
}

func TestConfigCacheAbsentOrCorruptIsIgnored(t *testing.T) {
	sup := testSupervisor(t, agentBootEngine{ID: "race-0", Role: "race"})
	if _, ok := sup.readCache(); ok {
		t.Fatal("a cache was read where no file exists")
	}
	if err := os.WriteFile(sup.cachePath(), []byte("{not json"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, ok := sup.readCache(); ok {
		t.Fatal("an unparseable cache was accepted")
	}
}

// A node with engines declared but nothing running has to say so, otherwise a
// front cannot tell "waiting for a config" from "engine is fine".
func TestConfigStateReportsDeclaredEnginesBeforeAnyRun(t *testing.T) {
	sup := testSupervisor(t,
		agentBootEngine{ID: "race-0", Role: "race", ListenPort: 12314},
		agentBootEngine{ID: "hoard-0", Role: "hoard", ListenPort: 12313})

	st := sup.ConfigState()
	if st.Source != agentwire.ConfigSourceNone {
		t.Fatalf("source = %q, want %q", st.Source, agentwire.ConfigSourceNone)
	}
	if len(st.Engines) != 2 {
		t.Fatalf("%d engines reported, want 2", len(st.Engines))
	}
	race := st.Engines["race-0"]
	if race.State != agentwire.EngineStatePending || race.Role != "race" || race.ListenPort != 12314 {
		t.Fatalf("race-0 reported as %+v", race)
	}

	// An engine whose last apply failed carries the reason, so the operator
	// reads it in the agents view instead of hunting through the node's log.
	sup.lastErr["hoard-0"] = "engine dir: permission denied"
	hoard := sup.ConfigState().Engines["hoard-0"]
	if hoard.State != agentwire.EngineStateError || hoard.Error == "" {
		t.Fatalf("a failed engine was not reported as such: %+v", hoard)
	}
}

// The front composes from a file that also declares the local engines; the
// agent's own file is only a fallback for a node that has not moved to the
// pushed model yet.
func TestLocalConfigFallbackBuildsAPayloadFromTheNodesOwnFile(t *testing.T) {
	cfg := config.DefaultConfig()
	cfg.Daemon.DataDir = t.TempDir()
	cfg.SourcePath = filepath.Join(cfg.Daemon.DataDir, "default.toml")
	cfg.Engines = []config.EngineConfig{{
		ID: "race-0", Role: "race",
		SessionConfig: config.SessionConfig{ListenPort: 12314, MaxConnections: 4000},
	}}
	cfg.AnnouncePasskeys = map[string]string{"tracker.example.net": "abc"}

	sup := newEngineSupervisor(context.Background(), cfg, []agentBootEngine{
		{ID: "race-0", Role: "race", ListenPort: 12314},
	}, nil)

	p, ok := sup.localConfig(cfg)
	if !ok {
		t.Fatal("a node declaring an engine in its own file got no fallback config")
	}
	if len(p.Engines) != 1 || p.Engines[0].Session.MaxConnections != 4000 {
		t.Fatalf("the file's session config did not reach the payload: %+v", p.Engines)
	}
	if p.Announce.Passkeys["tracker.example.net"] != "abc" {
		t.Fatal("the file's announce overrides were dropped")
	}
	if p.Revision == 0 {
		t.Fatal("the fallback payload has no revision, so a front could never see it change")
	}
}
