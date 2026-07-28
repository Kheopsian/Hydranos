package engine

import (
	"fmt"

	"github.com/Kheopsian/hydra/internal/engine/grpcclient"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// Compile-time proof that both transports satisfy the one narrow interface the
// engine logic (hoard.go/race.go) depends on. This is the crux of the pivot:
// "local" and "distant" are interchangeable EngineClients, so the front never
// branches on where a session actually runs.
var (
	_ EngineClient = (*ltclient.Client)(nil)
	_ EngineClient = (*grpcclient.Client)(nil)
)

// AgentEndpoint describes how to reach one engine session's data plane.
// Transport is deducible from which fields are set (unix socket vs host:port).
type AgentEndpoint struct {
	Transport string // "local" (default) | "grpc"
	Socket    string // local: hydra-engine unix socket path
	Addr      string // grpc: HydraAgent host:port
	Engine    string // grpc: "race" | "hoard"
	Token     string // grpc: bearer token
	TLSCa     string // grpc: CA cert path (empty = plaintext)
}

// NewEngineClient builds the EngineClient for one session, local or remote. The
// caller wires the result into HoardEngine/RaceEngine.SetClient exactly as it
// wires a local ltclient today.
func NewEngineClient(ep AgentEndpoint) (EngineClient, error) {
	switch ep.Transport {
	case "", "local":
		return ltclient.Connect(ep.Socket)
	case "grpc":
		return grpcclient.New(grpcclient.Config{
			Addr:   ep.Addr,
			Engine: ep.Engine,
			Token:  ep.Token,
			TLSCa:  ep.TLSCa,
		})
	default:
		return nil, fmt.Errorf("engine: unknown agent transport %q", ep.Transport)
	}
}
