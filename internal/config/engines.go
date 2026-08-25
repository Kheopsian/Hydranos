package config

import (
	"fmt"
	"strings"
)

// EngineConfig describes one engine a node hosts (Option A: a node runs an
// arbitrary set of engines). It embeds SessionConfig so a [[engine]] block
// carries the same tunables as the legacy [race]/[hoard] sections, plus an
// arbitrary id and a role.
type EngineConfig struct {
	ID   string `toml:"id" json:"id"`
	Role string `toml:"role" json:"role"` // "race" | "hoard"
	SessionConfig
}

// localAgentEngines returns the engines declared as [[agent]] entries that run
// HERE -- the ones with no addr.
//
// This is where the config is going now that one agent means one engine: a
// single [[agent]] array holding every node, with addr present meaning "reached
// over the network" and absent meaning "started here". [race] and [hoard]
// become what they already half were, fleet-wide profiles per role.
//
// ADDITIVE on purpose, unlike [[engine]] blocks. Those replace [race]/[hoard]
// the moment one exists, which is a rule nothing states and that silently
// disabled half a config -- the trap this array is meant to replace, not
// inherit. An entry whose id matches an engine already resolved replaces that
// one instead of colliding with it, so a node can override its own race engine
// without restating the rest.
//
// A role is required. Without it an [[agent]] entry that simply forgot its addr
// would be started as an engine here, which is a remote node quietly turned
// into a local one.
func (c *HydraConfig) localAgentEngines() []EngineConfig {
	var out []EngineConfig
	for i := range c.Agents {
		ag := &c.Agents[i]
		if strings.TrimSpace(ag.Addr) != "" || strings.TrimSpace(ag.Role) == "" {
			continue
		}
		id := strings.TrimSpace(ag.EngineID)
		if id == "" {
			id = strings.TrimSpace(ag.Name)
		}
		out = append(out, EngineConfig{ID: id, Role: ag.Role, SessionConfig: ag.Session})
	}
	return out
}

// ResolveEngines is the single source of truth for the engines a process runs.
// If [[engine]] blocks are present they win; otherwise the legacy [race]/[hoard]
// singletons are synthesised into two engines ("race"/"hoard"), so an existing
// default.toml keeps working unchanged. It validates unique ids, valid roles and
// unique listen ports (a duplicate port would leave an engine dead at boot).
func (c *HydraConfig) ResolveEngines() ([]EngineConfig, error) {
	var engines []EngineConfig
	if len(c.Engines) > 0 {
		engines = append(engines, c.Engines...)
	} else {
		engines = []EngineConfig{
			{ID: "race", Role: "race", SessionConfig: c.Race},
			{ID: "hoard", Role: "hoard", SessionConfig: c.Hoard},
		}
	}
	for _, le := range c.localAgentEngines() {
		replaced := false
		for i := range engines {
			if engines[i].ID == le.ID {
				engines[i] = le
				replaced = true
				break
			}
		}
		if !replaced {
			engines = append(engines, le)
		}
	}

	seenID := make(map[string]bool, len(engines))
	seenPort := make(map[int]bool, len(engines))
	for i := range engines {
		e := &engines[i]
		if e.ID == "" {
			return nil, fmt.Errorf("engine %d: empty id", i)
		}
		if e.Role != "race" && e.Role != "hoard" {
			return nil, fmt.Errorf("engine %q: role must be \"race\" or \"hoard\", got %q", e.ID, e.Role)
		}
		if seenID[e.ID] {
			return nil, fmt.Errorf("duplicate engine id %q", e.ID)
		}
		seenID[e.ID] = true
		if e.ListenPort != 0 {
			if seenPort[e.ListenPort] {
				return nil, fmt.Errorf("engine %q: listen_port %d already used by another engine", e.ID, e.ListenPort)
			}
			seenPort[e.ListenPort] = true
		}
	}
	return engines, nil
}
