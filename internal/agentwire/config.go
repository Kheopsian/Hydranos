package agentwire

import (
	"encoding/json"
	"hash/fnv"

	"github.com/Kheopsian/hydra/internal/config"
)

// Config distribution: an agent boots knowing only its own identity (engine
// id, role, listen port, IPv6) and the front composes and pushes everything
// else -- session tuning, egress, choking, disk slots and the announce
// overrides -- with MethodApplyConfig. This file is that payload.
//
// config.SessionConfig travels verbatim rather than being mirrored by a wire
// twin, for the same reason ImportStateFileParams keeps its record opaque: a
// second declaration of the same contract is free to drift from the first.
// The json tags on those structs mirror their toml tags, so a key reads the
// same in default.toml, on the wire, and in the agent's cache file.

// Values for ConfigState.Source: where the config an agent is running came
// from. "cache" and "local" both mean the front has not been heard from yet,
// which is what a front-side reconciler acts on.
const (
	ConfigSourceFront = "front" // pushed by a front in this process lifetime
	ConfigSourceCache = "cache" // replayed from the last pushed config on disk
	ConfigSourceLocal = "local" // legacy: full session config from the agent's own TOML
	ConfigSourceNone  = "none"  // nothing applied yet: engines are declared but down
)

// Values for EngineState.State.
const (
	EngineStateRunning = "running" // engine process up and serving
	EngineStatePending = "pending" // declared, waiting for a config to start from
	EngineStateError   = "error"   // last apply failed; Error carries why
)

// AgentEngineConfig is one engine's complete session config as composed by the
// front. ListenPort and EnableIPv6 are carried but NOT authoritative: the
// agent overlays its own boot identity over them before use, so a front-side
// mistake can never move an agent's listen port.
type AgentEngineConfig struct {
	ID      string               `json:"id"`
	Role    string               `json:"role"`
	Session config.SessionConfig `json:"session"`
}

// AnnounceConfigWire carries all four per-tracker announce override families.
// The older MethodSetAnnounceOverride only ever carried two of them (passkey
// and client spoof), which is why secondary-stats and ip-modes set on a front
// never reached its agents.
type AnnounceConfigWire struct {
	Passkeys       map[string]string          `json:"passkeys"`
	Clients        map[string]ClientSpoofWire `json:"clients"`
	SecondaryStats map[string]string          `json:"secondary_stats"`
	IPModes        map[string]string          `json:"ip_modes"`
}

// ApplyConfigParams is the params envelope for MethodApplyConfig: one whole
// node's configuration, replacing whatever it was running.
type ApplyConfigParams struct {
	Revision uint64              `json:"revision"`
	Engines  []AgentEngineConfig `json:"engines"`
	Announce AnnounceConfigWire  `json:"announce"`
}

// ComputeRevision derives the revision from the payload's content, so the same
// configuration always yields the same number on both ends. A counter would
// need durable state on the front and would make every restart look like a
// change to every agent; a content hash lets the reconciler compare what an
// agent reports against what it would send and stay quiet when they match.
//
// Callers must keep Engines in a stable order (the composer sorts by id):
// json.Marshal already sorts map keys, but it preserves slice order.
func (p ApplyConfigParams) ComputeRevision() uint64 {
	c := p
	c.Revision = 0
	b, err := json.Marshal(c)
	if err != nil {
		return 0
	}
	h := fnv.New64a()
	_, _ = h.Write(b)
	return h.Sum64()
}

// EngineState is one engine's status in a ConfigState reply.
type EngineState struct {
	Role       string `json:"role"`
	State      string `json:"state"`
	ListenPort int    `json:"listen_port"`
	Error      string `json:"error,omitempty"`
}

// ConfigState is the reply to MethodGetConfigState: what config this node is
// running and how each of its engines took it. The front polls it to detect an
// agent that restarted and came back on a stale (or no) config.
type ConfigState struct {
	Revision uint64                 `json:"revision"`
	Source   string                 `json:"source"`
	Engines  map[string]EngineState `json:"engines"`
}
