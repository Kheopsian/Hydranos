package api

import "github.com/Kheopsian/hydra/internal/config"

// EngineHost is the process that actually runs this node's extra engines.
//
// The API can write engines.json on its own, and until now that was all it did:
// the answer to "add an engine" was a config file and a restart banner. Starting
// one means a Typhon process, a store, an announcer and an agent registration,
// none of which belong in a HTTP handler -- so the handler asks whoever owns
// them. nil on a front-only node, which hosts no engine to add one to.
type EngineHost interface {
	// AddEngine starts an engine and makes it addressable as its own agent.
	AddEngine(ec config.EngineConfig) error
	// RemoveEngine stops one and unregisters it.
	RemoveEngine(id string) error
}

// SetEngineHost wires the owner of this node's engines. Without it the engine
// endpoints keep their old behaviour -- write the config, ask for a restart --
// rather than failing, because that is still the truth on a node that has no
// engine host.
func (s *Server) SetEngineHost(h EngineHost) { s.engineHost = h }
