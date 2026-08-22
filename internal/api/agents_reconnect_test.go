package api

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
)

func TestDesiredRemoteAgentsMergesConfigAndPersisted(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(agentsFile(dir), []byte(`{
  "ui-only": {"addr": "10.0.0.1:9090", "token": "ui"},
  "shared": {"addr": "10.0.0.2:9090", "token": "json"}
}`), 0644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(removedFile(dir), []byte(`{
  "deleted": {"addr": "10.0.0.9:9090"}
}`), 0644); err != nil {
		t.Fatal(err)
	}

	s := &Server{config: &config.HydraConfig{
		Daemon: config.DaemonConfig{DataDir: dir},
		Agents: []config.AgentConfig{
			{Name: "toml", Addr: "10.0.0.3:9090", Token: "toml"},
			{Name: "shared", Addr: "10.0.0.4:9090", Token: "toml-wins"},
			{Name: "no-addr"},
			{Addr: "10.0.0.5:9090"},
		},
	}}

	got := s.desiredRemoteAgents()
	if len(got) != 3 {
		t.Fatalf("got %d agents, want 3: %#v", len(got), got)
	}
	if a := got["toml"]; a.Addr != "10.0.0.3:9090" || a.Token != "toml" {
		t.Fatalf("toml agent = %#v", a)
	}
	if a := got["shared"]; a.Addr != "10.0.0.4:9090" || a.Token != "toml-wins" {
		t.Fatalf("config should win over agents.json for shared: %#v", a)
	}
	if a := got["ui-only"]; a.Addr != "10.0.0.1:9090" || a.Token != "ui" {
		t.Fatalf("ui-only agent = %#v", a)
	}
	if _, ok := got["deleted"]; ok {
		t.Fatal("soft-deleted agent must not be in desired set")
	}
}

func TestDesiredRemoteAgentsEmptyWhenNothingConfigured(t *testing.T) {
	s := &Server{config: &config.HydraConfig{
		Daemon: config.DaemonConfig{DataDir: t.TempDir()},
	}}
	if got := s.desiredRemoteAgents(); len(got) != 0 {
		t.Fatalf("got %#v, want empty map", got)
	}
}

func TestDesiredRemoteAgentsReadsPersistedFileEachCall(t *testing.T) {
	dir := t.TempDir()
	s := &Server{config: &config.HydraConfig{
		Daemon: config.DaemonConfig{DataDir: dir},
	}}
	if got := s.desiredRemoteAgents(); len(got) != 0 {
		t.Fatalf("initial set = %#v, want empty", got)
	}
	entry := map[string]agentStore{"late": {Addr: "10.0.0.8:9090"}}
	data, err := json.Marshal(entry)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "agents.json"), data, 0644); err != nil {
		t.Fatal(err)
	}
	got := s.desiredRemoteAgents()
	if len(got) != 1 || got["late"].Addr != "10.0.0.8:9090" {
		t.Fatalf("after write got %#v", got)
	}
}

func TestRemoteAgentOnline(t *testing.T) {
	if remoteAgentOnline(nil) {
		t.Fatal("nil agent reported online")
	}
	if remoteAgentOnline(&remoteAgent{}) {
		t.Fatal("agent with no engines reported online")
	}
	if remoteAgentOnline(&remoteAgent{engines: []remoteEngine{{}}}) {
		t.Fatal("agent with nil client reported online")
	}
}
