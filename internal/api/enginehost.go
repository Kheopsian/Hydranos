package api

import "github.com/Kheopsian/hydra/internal/config"

// EngineHost is the process that actually runs this node's extra engines.
//
// The API can write a config file on its own, and until now that was all it did:
// the answer to "add an engine" was a file and a restart banner. Starting one
// means a Typhon process, a store, an announcer and an agent registration, none
// of which belong in a HTTP handler -- so the handler asks whoever owns them.
// nil on a front-only node, which hosts no engine to add one to.
type EngineHost interface {
	// AddEngine starts an engine and makes it addressable as its own agent.
	AddEngine(ec config.EngineConfig) error
	// RemoveEngine stops one and unregisters it.
	RemoveEngine(id string) error
	// InboundAccepted counts peers that connected TO one of these engines, and
	// SampleServedInfoHash names a torrent it holds. Both are what the
	// reachability probe needs, and neither can be answered from the front: the
	// engine is the only thing that knows. Without them an extra engine had no
	// probe at all and its vertex stayed amber forever.
	InboundAccepted(id string) (int64, error)
	SampleServedInfoHash(id string) string
	// Engines reports what is RUNNING, which is not the same question as what
	// a config file says. Listing the file was how a failed start still showed
	// up as an engine, and how an engine started from a hand-written [[agent]]
	// entry showed up as nothing at all.
	Engines() []RunningEngine
}

// RunningEngine is one live extra engine, as the Agents page shows it.
type RunningEngine struct {
	ID            string `json:"id"`
	Role          string `json:"role"`
	ListenPort    int    `json:"listen_port"`
	BindInterface string `json:"bind_interface"`
}

// SetEngineHost wires the owner of this node's engines. Without it the engine
// endpoints keep their old behaviour -- write the config, ask for a restart --
// rather than failing, because that is still the truth on a node that has no
// engine host.
func (s *Server) SetEngineHost(h EngineHost) { s.engineHost = h }
