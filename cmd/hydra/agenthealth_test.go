package main

import (
	"encoding/json"
	"net/http/httptest"
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
)

// healthSupervisor builds a supervisor declaring the given engines, with the
// subset in lives registered as running.
func healthSupervisor(boot []agentBootEngine, lives ...*liveEngine) *engineSupervisor {
	sup := &engineSupervisor{
		boot:    boot,
		lives:   map[string]*liveEngine{},
		lastErr: map[string]string{},
	}
	for _, le := range lives {
		sup.lives[le.id] = le
	}
	return sup
}

func healthResponse(t *testing.T, method string, sup *engineSupervisor) (int, map[string]interface{}) {
	t.Helper()
	w := httptest.NewRecorder()
	agentHealthHandler(sup)(w, httptest.NewRequest(method, "/health", nil))
	var body map[string]interface{}
	_ = json.Unmarshal(w.Body.Bytes(), &body)
	return w.Code, body
}

func TestAgentHealthOKWithNoEngines(t *testing.T) {
	code, body := healthResponse(t, "GET", healthSupervisor(nil))
	if code != 200 {
		t.Fatalf("got %d, want 200", code)
	}
	if body["status"] != "healthy" || body["mode"] != "agent-only" {
		t.Fatalf("unexpected body: %v", body)
	}
}

// A node whose engine died still answers gRPC, so the probe has to fail on the
// engine rather than on the process being alive.
func TestAgentHealthFailsOnADeadEngine(t *testing.T) {
	boot := []agentBootEngine{{ID: "hoard-0", Role: "hoard"}}
	sup := healthSupervisor(boot, &liveEngine{id: "hoard-0", role: "hoard"})
	code, body := healthResponse(t, "GET", sup)
	if code != 503 {
		t.Fatalf("got %d, want 503", code)
	}
	if body["status"] != "unhealthy" {
		t.Fatalf("status = %v, want unhealthy", body["status"])
	}
	engines, _ := body["engines"].([]interface{})
	if len(engines) != 1 {
		t.Fatalf("engines = %v, want one entry", body["engines"])
	}
	e := engines[0].(map[string]interface{})
	if e["ok"] != false || e["id"] != "hoard-0" || e["error"] == "" {
		t.Fatalf("engine entry does not name the failure: %v", e)
	}
}

// An engine the front has not configured yet runs nothing, and a probe that
// only listed the live engines would call that an empty, healthy node.
func TestAgentHealthFailsOnAnUnconfiguredEngine(t *testing.T) {
	sup := healthSupervisor([]agentBootEngine{{ID: "race-0", Role: "race"}})
	code, body := healthResponse(t, "GET", sup)
	if code != 503 {
		t.Fatalf("got %d, want 503", code)
	}
	engines, _ := body["engines"].([]interface{})
	if len(engines) != 1 {
		t.Fatalf("engines = %v, want the declared engine", body["engines"])
	}
	e := engines[0].(map[string]interface{})
	if e["ok"] != false || e["id"] != "race-0" || e["error"] == "" {
		t.Fatalf("engine entry does not say it is unconfigured: %v", e)
	}
}

func TestAgentHealthRejectsWrites(t *testing.T) {
	if code, _ := healthResponse(t, "POST", healthSupervisor(nil)); code != 405 {
		t.Fatalf("POST /health returned %d, want 405", code)
	}
}

func TestResolveHealthAddr(t *testing.T) {
	cfg := &config.HydraConfig{}
	cfg.Daemon.APIHost, cfg.Daemon.APIPort = "0.0.0.0", 8199

	cases := []struct {
		flag, want string
	}{
		{"", "0.0.0.0:8199"},
		{"off", ""},
		{"OFF", ""},
		{"none", ""},
		{"127.0.0.1:9000", "127.0.0.1:9000"},
	}
	for _, c := range cases {
		if got := resolveHealthAddr(c.flag, cfg); got != c.want {
			t.Errorf("resolveHealthAddr(%q) = %q, want %q", c.flag, got, c.want)
		}
	}

	// No API port configured: nothing to fall back to, so serve nothing rather
	// than bind :0 and hand the orchestrator an address it cannot guess.
	noPort := &config.HydraConfig{}
	if got := resolveHealthAddr("", noPort); got != "" {
		t.Errorf("resolveHealthAddr with no api_port = %q, want empty", got)
	}
}
