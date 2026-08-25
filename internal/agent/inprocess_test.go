package agent

import (
	"context"
	"strings"
	"testing"

	"github.com/Kheopsian/hydra/internal/agentpb"
	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/engine"
)

// TestInProcessStubRunsTheRealHandlers is the point of the whole adapter: a
// local engine must answer through the SAME switch a remote one does. If this
// ever needed its own handler, the local and remote paths could answer
// differently, which is the divergence making local engines agents is meant to
// remove.
func TestInProcessStubRunsTheRealHandlers(t *testing.T) {
	srv := NewServer(map[string]engine.EngineClient{}, t.TempDir(), "")
	srv.DeclareEngines([]agentwire.EngineDescriptor{
		{ID: "local-race", Role: "race"},
		{ID: "local-hoard", Role: "hoard"},
	})
	stub := InProcessStub(srv)
	if stub == nil {
		t.Fatal("no stub for a real server")
	}

	rep, err := stub.Call(context.Background(), &agentpb.CallRequest{Method: agentwire.MethodListEngines})
	if err != nil {
		t.Fatalf("list_engines through the in-process stub: %v", err)
	}
	if rep.Error != "" {
		t.Fatalf("handler reported: %s", rep.Error)
	}
	// The reply carries what the real handler produced, not something this
	// adapter invented.
	if !strings.Contains(string(rep.Result), "local-race") ||
		!strings.Contains(string(rep.Result), "local-hoard") {
		t.Errorf("declared engines missing from the reply: %s", rep.Result)
	}
}

// A nil server must not yield a stub that panics on first use.
func TestInProcessStubOfNilServerIsNil(t *testing.T) {
	if InProcessStub(nil) != nil {
		t.Error("a nil server produced a non-nil stub, which would panic on the first call")
	}
}

// Subscribe must fail loudly. Faking it would deliver every event twice: an
// in-process engine's events already reach the front through its own hub, so a
// second server-shaped stream is duplication, not a fallback.
func TestInProcessStubRefusesSubscribe(t *testing.T) {
	srv := NewServer(map[string]engine.EngineClient{}, t.TempDir(), "")
	_, err := InProcessStub(srv).Subscribe(context.Background(), &agentpb.SubscribeRequest{})
	if err == nil {
		t.Fatal("Subscribe succeeded in process: events would arrive twice, once from the hub and once from here")
	}
	if !strings.Contains(err.Error(), "hub") {
		t.Errorf("error does not say where events actually come from: %v", err)
	}
}
