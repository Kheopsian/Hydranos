package api

import (
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
)

// The header lost its IPv6 address because the v6 measurement was gated on a
// flag ComposeSession had just set to false. That is not a bug in
// ComposeSession: it composes what the front PUSHES to an agent, where the
// listen port and IPv6 belong to the agent, so it zeroes them ON PURPOSE.
//
// This pins the difference, because the two functions read alike and answer
// different questions.
func TestResolvedEngineKeepsWhatComposeSessionDeliberatelyZeroes(t *testing.T) {
	cfg := &config.HydraConfig{}
	cfg.Race.ListenPort = 16171
	cfg.Race.EnableIPv6 = true
	cfg.Hoard.ListenPort = 16172
	cfg.Hoard.EnableIPv6 = true

	ec, ok := resolvedEngine(cfg, "hoard")
	if !ok {
		t.Fatal("the hoard engine did not resolve")
	}
	if !ec.EnableIPv6 {
		t.Error("enable_ipv6 = false on a node whose config says true: no v6 address would ever be measured")
	}
	if ec.ListenPort != 16172 {
		t.Errorf("listen_port = %d, want 16172", ec.ListenPort)
	}

	// The push really does zero them -- if this ever stops being true, the
	// comment above is wrong and so is the reason this helper exists.
	pushed, err := cfg.ComposeSession(LocalAgentNameFor("hoard"), "hoard", "hoard")
	if err != nil {
		t.Fatal(err)
	}
	if pushed.EnableIPv6 || pushed.ListenPort != 0 {
		t.Errorf("ComposeSession no longer zeroes the node's own keys (%v, %d)", pushed.EnableIPv6, pushed.ListenPort)
	}
}

// An [[agent]] entry of this node overrides the profile, and the measurement
// has to follow it -- an engine given its own tunnel is the whole point.
func TestResolvedEngineFollowsALocalOverride(t *testing.T) {
	cfg := &config.HydraConfig{}
	cfg.Hoard.ListenPort = 16172
	cfg.Hoard.BindInterface = "tun0"
	cfg.Hoard.EnableIPv6 = true
	cfg.Agents = []config.AgentConfig{{
		Name: "local-vpn7", Role: "hoard", EngineID: "vpn7",
		Session: map[string]interface{}{"listen_port": int64(26991), "bind_interface": "wg7"},
	}}

	ec, ok := resolvedEngine(cfg, "vpn7")
	if !ok {
		t.Fatal("the extra engine did not resolve")
	}
	if ec.BindInterface != "wg7" || ec.ListenPort != 26991 {
		t.Errorf("engine = %q/%d, want wg7/26991", ec.BindInterface, ec.ListenPort)
	}
	if !ec.EnableIPv6 {
		t.Error("the entry dropped the profile's enable_ipv6")
	}
}
