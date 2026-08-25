package api

import (
	"testing"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// streamingClient is an agent client that CAN stream.
type streamingClient struct {
	AgentClient
	handler    func(ltclient.Event)
	subscribed bool
	failWith   error
}

func (c *streamingClient) SetEventHandler(h func(ltclient.Event)) { c.handler = h }
func (c *streamingClient) SubscribeEvents() error {
	if c.failWith != nil {
		return c.failWith
	}
	c.subscribed = true
	return nil
}

// TestLocalEnginesAreNotSubscribed is the guard on an exclusion that is made by
// construction rather than by a condition. localAgentClient does not implement
// eventStreamer, so it cannot be subscribed even by mistake -- its events
// already reach the front through the engine's own hub, and a second
// server-shaped stream would deliver every one of them twice.
//
// The test is here because "it does not implement the interface" is exactly the
// kind of property someone restores by adding two convenient methods.
func TestLocalEnginesAreNotSubscribed(t *testing.T) {
	var local AgentClient = newLocalAgentClient("hoard", &fakeEngine{}, &countingAgent{})
	if _, ok := local.(eventStreamer); ok {
		t.Fatal("the in-process client can be subscribed: its events would arrive twice, once from the hub and once from the stream")
	}
	s := &Server{}
	// Registering and subscribing must simply do nothing, not panic.
	ra := &remoteAgent{name: LocalAgentHoard, local: true,
		engines: []remoteEngine{{id: "hoard", role: "hoard", client: local}}}
	s.subscribeAgentRows(ra)
}

// A remote agent must be subscribed, or the whole change is inert and the poll
// silently keeps paying for a full re-listing.
func TestRemoteAgentsAreSubscribed(t *testing.T) {
	s := &Server{}
	cl := &streamingClient{}
	ra := &remoteAgent{name: "seedbox",
		engines: []remoteEngine{{id: "hoard-0", role: "hoard", client: cl}}}
	s.subscribeAgentRows(ra)
	if !cl.subscribed {
		t.Fatal("a remote agent was not subscribed: the cache keeps re-listing it in full")
	}
	if cl.handler == nil {
		t.Fatal("no handler installed, so the stream would be received and discarded")
	}
}

// A stream that refuses to open must leave the agent polled, not broken. This
// is the fallback that keeps the change safe.
func TestAFailedSubscriptionFallsBackToPolling(t *testing.T) {
	s := &Server{}
	cl := &streamingClient{failWith: errAlways}
	ra := &remoteAgent{name: "seedbox",
		engines: []remoteEngine{{id: "hoard-0", role: "hoard", client: cl}}}
	s.subscribeAgentRows(ra) // must not panic
	if cl.subscribed {
		t.Error("reported subscribed despite the error")
	}
}

type constErr string

func (e constErr) Error() string { return string(e) }

const errAlways = constErr("stream unavailable")
