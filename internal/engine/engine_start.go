package engine

import "github.com/Kheopsian/hydra/internal/config"

// StartSessionEngine spawns the Typhon engine for a session and returns a
// connected EngineClient.
func StartSessionEngine(cfg *config.SessionConfig, dataDir, socketPath string, isRace bool) (*EngineProcess, error) {
	return StartSessionEngineWithBindings(cfg, dataDir, socketPath, isRace, nil)
}

// StartSessionEngineWithBindings is the multi-binding variant. When `bindings`
// is non-empty they are forwarded to the engine's EngineConfig so each binding
// gets its own TCP listener with its own peer_id.
func StartSessionEngineWithBindings(cfg *config.SessionConfig, dataDir, socketPath string, isRace bool, bindings []EngineBinding) (*EngineProcess, error) {
	var ec EngineConfig
	if isRace {
		ec = BuildRaceConfig(cfg, dataDir)
	} else {
		ec = BuildHoardConfig(cfg, dataDir)
	}
	if len(bindings) > 0 {
		ec.Bindings = bindings
	}
	return StartEngineProcess(ec, socketPath)
}

// EngineBindingsFromGo converts Hydra's []Binding (used by HoardAnnouncer) into
// the JSON-serialised []EngineBinding accepted by the engine.
func EngineBindingsFromGo(bindings []Binding) []EngineBinding {
	out := make([]EngineBinding, len(bindings))
	for i, b := range bindings {
		out[i] = EngineBinding{
			ID:           uint32(b.ID),
			PeerID:       b.PeerID,
			ListenAddr:   b.ListenAddr,
			ListenPort:   uint16(b.ListenPort),
			AnnouncePort: uint16(b.AnnouncePort),
			PublicIP:     b.PublicIP,
			Fwmark:       b.Fwmark,
		}
	}
	return out
}
