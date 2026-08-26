package config

import (
	"fmt"

	toml "github.com/pelletier/go-toml/v2"
)

// Agent engines take their configuration from the front, not from a file on
// their own host. This file composes what a front sends: the [race] / [hoard]
// sections become fleet-wide PROFILES for remote engines of that role, and an
// [[agent.engine]] block under an [[agent]] applies sparse exceptions on top.
//
//	[race]
//	max_connections = 4000
//
//	[[agent]]
//	name = "de-1"
//	addr = "10.0.0.5:9090"
//	  [[agent.engine]]
//	  id = "race-0"
//	  announce_proxy = "socks5h://user:pass@127.0.0.1:1080"
//
// Two keys in an override block select the engine instead of configuring it.
const (
	overrideKeyID   = "id"
	overrideKeyRole = "role"
)

// ProfileForRole returns the fleet profile a remote engine of this role starts
// from.
func (c *HydraConfig) ProfileForRole(role string) (SessionConfig, error) {
	switch role {
	case "race":
		return c.Race, nil
	case "hoard":
		return c.Hoard, nil
	}
	return SessionConfig{}, fmt.Errorf("unknown engine role %q (want \"race\" or \"hoard\")", role)
}

// AgentByName returns the [[agent]] block with this name, or nil.
func (c *HydraConfig) AgentByName(name string) *AgentConfig {
	for i := range c.Agents {
		if c.Agents[i].Name == name {
			return &c.Agents[i]
		}
	}
	return nil
}

// ComposeSession builds the session config a given engine of a given agent
// should run: the role profile with that engine's [[agent.engine]] overrides
// merged over it.
//
// listen_port and enable_ipv6 are zeroed in the result. They belong to the
// agent -- it is the side that knows which port its VPN forwards and which
// interfaces it has -- and the agent overlays its own boot values over
// whatever arrives. Zeroing them here makes that ownership explicit on the
// wire instead of shipping a value that is silently ignored.
func (c *HydraConfig) ComposeSession(agentName, engineID, role string) (SessionConfig, error) {
	profile, err := c.ProfileForRole(role)
	if err != nil {
		return SessionConfig{}, err
	}
	// A race profile carries [race.custom_choking] and a hoard one may carry
	// [hoard.disk_slots]; neither means anything to the other role, and the
	// legacy [race]/[hoard] pair in an old config often has both filled in.
	if role != "race" {
		profile.CustomChoking = nil
	}
	if role != "hoard" {
		profile.DiskSlots = nil
	}

	// A locally-hosted entry keeps its own settings in [[agent]] session keys
	// rather than in an [[agent.engine]] block. Both are sparse overrides of
	// the same profile, so both are applied here: composing without this one
	// would push a config that silently reverts the engine's own interface to
	// whatever the fleet profile says, which is exactly the leak this series
	// exists to prevent.
	if ag := c.AgentByName(agentName); ag != nil && len(ag.Session) > 0 {
		profile, err = applySessionOverride(profile, ag.Session)
		if err != nil {
			return SessionConfig{}, fmt.Errorf("agent %q: %w", agentName, err)
		}
	}
	if ov := c.engineOverride(agentName, engineID); ov != nil {
		profile, err = applySessionOverride(profile, ov)
		if err != nil {
			return SessionConfig{}, fmt.Errorf("agent %q engine %q: %w", agentName, engineID, err)
		}
	}

	profile.ListenPort = 0
	profile.EnableIPv6 = false
	return profile, nil
}

// engineOverride finds the override block for one engine of one agent.
func (c *HydraConfig) engineOverride(agentName, engineID string) map[string]interface{} {
	ag := c.AgentByName(agentName)
	if ag == nil {
		return nil
	}
	for _, ov := range ag.EngineOverrides {
		if id, _ := ov[overrideKeyID].(string); id == engineID {
			return ov
		}
	}
	return nil
}

// applySessionOverride merges sparse keys over a profile by going through
// TOML: the profile is encoded to a generic map, the override keys are merged
// into it, and the result is decoded back into a SessionConfig. Merging on the
// encoded form is what makes the override sparse -- only the keys actually
// written are touched -- and it reuses the exact same key names and value
// types the config file already uses, so an override cannot mean something
// different from the same line written in [race].
func applySessionOverride(profile SessionConfig, override map[string]interface{}) (SessionConfig, error) {
	encoded, err := toml.Marshal(profile)
	if err != nil {
		return SessionConfig{}, err
	}
	base, err := ParseTOMLMap(encoded)
	if err != nil {
		return SessionConfig{}, err
	}
	if base == nil {
		base = map[string]interface{}{}
	}
	for k, v := range override {
		if k == overrideKeyID || k == overrideKeyRole {
			continue
		}
		mergeTOMLValue(base, k, v)
	}
	merged, err := toml.Marshal(base)
	if err != nil {
		return SessionConfig{}, err
	}
	var out SessionConfig
	if err := toml.Unmarshal(merged, &out); err != nil {
		return SessionConfig{}, err
	}
	return out, nil
}

// mergeTOMLValue sets one key, recursing into sub-tables so an override of a
// single key inside [[agent.engine].custom_choking] does not wipe the rest of
// the profile's choking settings.
func mergeTOMLValue(dst map[string]interface{}, key string, val interface{}) {
	sub, isTable := val.(map[string]interface{})
	if !isTable {
		dst[key] = val
		return
	}
	cur, ok := dst[key].(map[string]interface{})
	if !ok {
		cur = map[string]interface{}{}
		dst[key] = cur
	}
	for k, v := range sub {
		mergeTOMLValue(cur, k, v)
	}
}

// AnnounceClientSpoofs returns the [announce_clients] table in the shape the
// engine layer wants, without that layer having to know the config type.
func (c *HydraConfig) AnnounceClientSpoofs() map[string]ClientSpoofConfig {
	out := make(map[string]ClientSpoofConfig, len(c.AnnounceClients))
	for host, s := range c.AnnounceClients {
		out[host] = s
	}
	return out
}
