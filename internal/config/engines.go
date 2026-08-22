package config

import "fmt"

// EngineConfig describes one engine a node hosts (Option A: a node runs an
// arbitrary set of engines). It embeds SessionConfig so a [[engine]] block
// carries the same tunables as the legacy [race]/[hoard] sections, plus an
// arbitrary id and a role.
type EngineConfig struct {
	ID   string `toml:"id" json:"id"`
	Role string `toml:"role" json:"role"` // "race" | "hoard"
	SessionConfig
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
