package api

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/gin-gonic/gin"
)

// fakeEngineHost stands in for the process that runs the engines.
type fakeEngineHost struct {
	added   []string
	removed []string
	addErr  error
}

func (f *fakeEngineHost) AddEngine(ec config.EngineConfig) error {
	if f.addErr != nil {
		return f.addErr
	}
	f.added = append(f.added, ec.ID)
	return nil
}
func (f *fakeEngineHost) RemoveEngine(id string) error {
	f.removed = append(f.removed, id)
	return nil
}

func engineTestServer(t *testing.T, host EngineHost) *Server {
	t.Helper()
	gin.SetMode(gin.TestMode)
	cfg := &config.HydraConfig{}
	cfg.Daemon.DataDir = t.TempDir()
	return &Server{config: cfg, engineHost: host}
}

func postEngine(t *testing.T, s *Server, body string) *httptest.ResponseRecorder {
	t.Helper()
	r := gin.New()
	r.POST("/engines", s.handleEnginesPost)
	w := httptest.NewRecorder()
	req := httptest.NewRequest(http.MethodPost, "/engines", strings.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	r.ServeHTTP(w, req)
	return w
}

// Adding an engine used to mean writing engines.json and showing a restart
// banner. It starts the engine now, and the answer says so -- a UI that still
// keyed on restart_required would otherwise ask for a restart nothing needs.
func TestAddingAnEngineStartsItInsteadOfAskingForARestart(t *testing.T) {
	host := &fakeEngineHost{}
	s := engineTestServer(t, host)

	w := postEngine(t, s, `{"id":"vpn7","role":"hoard"}`)
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200 (%s)", w.Code, w.Body.String())
	}
	var got map[string]any
	if err := json.Unmarshal(w.Body.Bytes(), &got); err != nil {
		t.Fatalf("body: %v (%s)", err, w.Body.String())
	}
	if rr, _ := got["restart_required"].(bool); rr {
		t.Errorf("restart_required = true for an engine that is already running")
	}
	if started, _ := got["started"].(bool); !started {
		t.Errorf("started = false, want true")
	}
	if len(host.added) != 1 || host.added[0] != "vpn7" {
		t.Errorf("engine host was asked to start %v, want [vpn7]", host.added)
	}
	engs, err := config.LoadExtraEngines(s.config.Daemon.DataDir)
	if err != nil || len(engs) != 1 || engs[0].ID != "vpn7" {
		t.Errorf("engines.json holds %v (err %v), want the new engine", engs, err)
	}
}

// An engine that cannot come up must not be written down. Persisting it would
// make it fail identically at every boot from here on, with the reason buried
// in the startup log instead of being the answer to this request.
func TestAnEngineThatFailsToStartIsNotPersisted(t *testing.T) {
	host := &fakeEngineHost{addErr: errFakeStart}
	s := engineTestServer(t, host)

	w := postEngine(t, s, `{"id":"vpn7","role":"hoard"}`)
	if w.Code != http.StatusInternalServerError {
		t.Fatalf("status = %d, want 500 (%s)", w.Code, w.Body.String())
	}
	engs, _ := config.LoadExtraEngines(s.config.Daemon.DataDir)
	if len(engs) != 0 {
		t.Errorf("engines.json holds %v after a failed start, want nothing", engs)
	}
}

// A node with no engine host -- front-only -- still has to answer the old way,
// or the UI would report an engine as running on a node that runs none.
func TestWithoutAnEngineHostTheAnswerStillAsksForARestart(t *testing.T) {
	s := engineTestServer(t, nil)
	s.engineHost = nil

	w := postEngine(t, s, `{"id":"vpn7","role":"hoard"}`)
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200 (%s)", w.Code, w.Body.String())
	}
	var got map[string]any
	_ = json.Unmarshal(w.Body.Bytes(), &got)
	if rr, _ := got["restart_required"].(bool); !rr {
		t.Errorf("restart_required = false with no engine host to start anything")
	}
}

// Deleting stops the engine, and the config is written first so a failure to
// stop cannot silently resurrect it at the next boot.
func TestDeletingAnEngineStopsIt(t *testing.T) {
	host := &fakeEngineHost{}
	s := engineTestServer(t, host)
	if w := postEngine(t, s, `{"id":"vpn7","role":"hoard"}`); w.Code != http.StatusOK {
		t.Fatalf("add: %d (%s)", w.Code, w.Body.String())
	}

	r := gin.New()
	r.DELETE("/engines/:id", s.handleEnginesDelete)
	w := httptest.NewRecorder()
	r.ServeHTTP(w, httptest.NewRequest(http.MethodDelete, "/engines/vpn7", nil))
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200 (%s)", w.Code, w.Body.String())
	}
	if len(host.removed) != 1 || host.removed[0] != "vpn7" {
		t.Errorf("engine host was asked to stop %v, want [vpn7]", host.removed)
	}
	if engs, _ := config.LoadExtraEngines(s.config.Daemon.DataDir); len(engs) != 0 {
		t.Errorf("engines.json still holds %v", engs)
	}
}

type fakeStartErr struct{}

func (fakeStartErr) Error() string { return "listen port already taken" }

var errFakeStart = fakeStartErr{}
