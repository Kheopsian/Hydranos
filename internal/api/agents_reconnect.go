package api

import (
	"log/slog"

	"github.com/Kheopsian/hydra/internal/config"
)

// desiredRemoteAgents merges TOML [[agent]] blocks with agents.json. Config
// wins on name collisions. Soft-deleted agents are left out: re-dialling one
// would undo a delete the user made from the Agents menu, and for a TOML agent
// the removed store is the only record that delete leaves.
//
// cfg is passed in rather than read from s.config so the reconciler can hand it
// the file as it stands now: an [[agent]] block added while the front runs is
// dialed on the next pass instead of at the next restart.
func (s *Server) desiredRemoteAgents(cfg *config.HydraConfig) map[string]agentStore {
	removed := loadRemovedStore(s.config.Daemon.DataDir)
	out := make(map[string]agentStore)
	for _, ag := range cfg.Agents {
		if why := agentConfigError(ag); why != "" {
			slog.Warn("[[agent]] block ignored, it cannot be dialed",
				"reason", why, "name", ag.Name, "addr", ag.Addr)
			continue
		}
		if _, gone := removed[ag.Name]; gone {
			continue
		}
		out[ag.Name] = agentStore{Addr: ag.Addr, Token: ag.Token, TLSCa: ag.TLSCa}
	}
	for name, a := range loadAgentStore(s.config.Daemon.DataDir) {
		if _, exists := out[name]; exists {
			continue
		}
		if _, gone := removed[name]; gone {
			continue
		}
		out[name] = a
	}
	return out
}

func remoteAgentOnline(ra *remoteAgent) bool {
	if ra == nil {
		return false
	}
	for _, e := range ra.engines {
		if e.client != nil && e.client.Ping() == nil {
			return true
		}
	}
	return false
}
