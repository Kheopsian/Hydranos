package agent

import (
	"context"
	"fmt"

	"google.golang.org/grpc"

	"github.com/Kheopsian/hydra/internal/agentpb"
)

// inProcessStub is the agent's gRPC client surface, satisfied by calling this
// process's own Server directly instead of dialling it.
//
// It exists so that an engine running here can be addressed exactly like one on
// another machine, without a listener, a port, a token or a TLS decision. The
// alternative -- serving on loopback and connecting back to ourselves -- is
// what the shard engines do today, and it works, but it means the monolith
// cannot reach its own engines until a socket is up, and it puts a real network
// stack under a call that never leaves the process.
//
// The encoding is deliberately NOT skipped here. Going through Call keeps the
// local path on the exact same param and result envelopes as the remote one, so
// a local agent cannot quietly answer differently from a remote agent -- which
// is the entire reason for making local engines agents in the first place. This
// is only ever used for cold, per-user-action calls; the hot path (listing,
// stats, per-torrent reads) bypasses this and goes straight to the engine,
// because at production scale that encoding costs about a gigabyte per list.
type inProcessStub struct{ srv *Server }

// InProcessStub adapts a Server into the client interface, for a caller in the
// same process. The returned value is safe for concurrent use: it holds nothing
// but the server pointer, and Server.Call is already the concurrent entry point
// for every remote caller.
func InProcessStub(srv *Server) agentpb.HydraAgentClient {
	if srv == nil {
		return nil
	}
	return &inProcessStub{srv: srv}
}

func (s *inProcessStub) Call(ctx context.Context, in *agentpb.CallRequest, _ ...grpc.CallOption) (*agentpb.CallReply, error) {
	return s.srv.Call(ctx, in)
}

// Subscribe is refused rather than faked. Event delivery for an in-process
// engine already happens through the engine's own hub, which the front wires
// directly; standing up a second, server-shaped path would deliver every event
// twice. A caller reaching this has taken the remote path for a local engine by
// mistake, and should hear about it instead of receiving a stream that silently
// never yields.
func (s *inProcessStub) Subscribe(context.Context, *agentpb.SubscribeRequest, ...grpc.CallOption) (grpc.ServerStreamingClient[agentpb.EventFrame], error) {
	return nil, fmt.Errorf("agent: Subscribe is not available in process; a local engine's events come from its own hub")
}
