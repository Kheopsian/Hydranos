package api

import (
	"context"
	"log/slog"
	"time"
)

const agentReconnectInterval = time.Minute

// StartAgentReconnectLoop retries dialing configured remote agents on a fixed
// interval. A front that boots before its agents are reachable dials once,
// fails, and would otherwise never try again; this loop keeps attempting until
// each desired agent is registered and answering Ping.
func (s *Server) StartAgentReconnectLoop(ctx context.Context) {
	go func() {
		ticker := time.NewTicker(agentReconnectInterval)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				s.reconnectRemoteAgents()
			}
		}
	}()
}

func (s *Server) reconnectRemoteAgents() {
	for name, spec := range s.desiredRemoteAgents() {
		if ra := s.remoteAgentByName(name); ra != nil && remoteAgentOnline(ra) {
			continue
		}
		if err := s.AddRemoteAgent(name, spec.Addr, spec.Token, spec.TLSCa); err != nil {
			slog.Debug("remote agent reconnect failed", "name", name, "addr", spec.Addr, "err", err)
			continue
		}
		slog.Info("remote agent registered", "name", name, "addr", spec.Addr)
	}
}

// desiredRemoteAgents merges TOML [[agent]] blocks with agents.json. Config
// wins on name collisions; agents_removed.json is not consulted.
func (s *Server) desiredRemoteAgents() map[string]agentStore {
	out := make(map[string]agentStore)
	for _, ag := range s.config.Agents {
		if ag.Name == "" || ag.Addr == "" {
			continue
		}
		out[ag.Name] = agentStore{Addr: ag.Addr, Token: ag.Token, TLSCa: ag.TLSCa}
	}
	for name, a := range loadAgentStore(s.config.Daemon.DataDir) {
		if _, exists := out[name]; exists {
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
