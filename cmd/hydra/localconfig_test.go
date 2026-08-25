package main

import (
	"testing"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine"
)

func slotFor(id, role string, port int) *localEngineSlot {
	cfg := &config.SessionConfig{ListenPort: port, EnableIPv6: true, MaxConnections: 100}
	return &localEngineSlot{id: id, role: role, cfg: cfg, ref: &engine.EngineRef{}}
}

// TestPushedConfigCannotStealTheListenPort is the one that would have bitten in
// production. ComposeSession zeroes listen_port and enable_ipv6 on the way out,
// because on a REMOTE node they belong to the agent -- it knows which port its
// VPN forwards. Here the front is the agent, so applying the pushed value
// verbatim would set this node's listen port to zero on the first reload and
// take it off the swarm.
func TestPushedConfigCannotStealTheListenPort(t *testing.T) {
	m := newLocalConfigManager(slotFor("race", "race", 16171))
	// A push that changes nothing except carrying the zeroed pair.
	m.ApplyConfig(agentwire.ApplyConfigParams{
		Revision: 7,
		Engines: []agentwire.AgentEngineConfig{{
			ID: "race", Role: "race",
			Session: config.SessionConfig{MaxConnections: 100}, // ListenPort 0, EnableIPv6 false
		}},
	})
	got := m.slots["race"].cfg
	if got.ListenPort != 16171 {
		t.Errorf("listen port became %d: the node just left the swarm", got.ListenPort)
	}
	if !got.EnableIPv6 {
		t.Error("enable_ipv6 was cleared by a push that never carried it")
	}
}

// A push identical to what is running must not restart anything: a restart
// drops every peer connection the engine holds, and the front re-pushes on
// every reconcile tick.
func TestIdenticalPushDoesNotRestart(t *testing.T) {
	m := newLocalConfigManager(slotFor("race", "race", 16171))
	same := config.SessionConfig{ListenPort: 16171, EnableIPv6: true, MaxConnections: 100}
	if !sameSession(*m.slots["race"].cfg, same) {
		t.Fatal("an unchanged session compared as different: every tick would restart the engine")
	}
	// proc stays nil, so a restart attempt would panic or error: reaching the
	// end proves none was made.
	st := m.ApplyConfig(agentwire.ApplyConfigParams{
		Revision: 1,
		Engines:  []agentwire.AgentEngineConfig{{ID: "race", Role: "race", Session: same}},
	})
	if st.Engines["race"].State != agentwire.EngineStateRunning {
		t.Errorf("state = %q after a no-op push", st.Engines["race"].State)
	}
}

// A push for an engine this manager does not own must be ignored, not applied
// to something else: the shards have their own.
func TestUnknownEngineIsIgnored(t *testing.T) {
	m := newLocalConfigManager(slotFor("race", "race", 16171))
	m.ApplyConfig(agentwire.ApplyConfigParams{
		Engines: []agentwire.AgentEngineConfig{{ID: "somebody-else", Role: "hoard",
			Session: config.SessionConfig{MaxConnections: 999}}},
	})
	if m.slots["race"].cfg.MaxConnections != 100 {
		t.Error("a push aimed at another engine was applied here")
	}
}

// The reported state must name the port actually in force, or the front shows a
// fleet as converged while a node runs something else.
func TestStateReportsTheRunningPort(t *testing.T) {
	m := newLocalConfigManager(slotFor("hoard", "hoard", 16172))
	st := m.ConfigState()
	if st.Engines["hoard"].ListenPort != 16172 || st.Engines["hoard"].Role != "hoard" {
		t.Errorf("state = %+v", st.Engines["hoard"])
	}
}
