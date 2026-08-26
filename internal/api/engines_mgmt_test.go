package api

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/gin-gonic/gin"
)

// fakeEngineHost stands in for the process that runs the engines.
type fakeEngineHost struct {
	added   []string
	removed []string
	running []RunningEngine
	addErr  error
}

func (f *fakeEngineHost) AddEngine(ec config.EngineConfig) error {
	if f.addErr != nil {
		return f.addErr
	}
	f.added = append(f.added, ec.ID)
	f.running = append(f.running, RunningEngine{ID: ec.ID, Role: ec.Role,
		ListenPort: ec.ListenPort, BindInterface: ec.BindInterface})
	return nil
}
func (f *fakeEngineHost) RemoveEngine(id string) error {
	f.removed = append(f.removed, id)
	for i, e := range f.running {
		if e.ID == id {
			f.running = append(f.running[:i], f.running[i+1:]...)
			break
		}
	}
	return nil
}
func (f *fakeEngineHost) Engines() []RunningEngine { return f.running }

const engineTestTOML = `[daemon]
data_dir = "%DIR%"

[race]
listen_port = 16171
max_connections = 4000

[hoard]
listen_port = 16172
max_connections = 12000
bind_interface = "tun0"
`

func engineTestServer(t *testing.T, host EngineHost) *Server {
	t.Helper()
	gin.SetMode(gin.TestMode)
	dir := t.TempDir()
	path := filepath.Join(dir, "default.toml")
	if err := os.WriteFile(path, []byte(strings.ReplaceAll(engineTestTOML, "%DIR%", dir)), 0644); err != nil {
		t.Fatal(err)
	}
	cfg, err := config.Reload(path)
	if err != nil {
		t.Fatal(err)
	}
	s := &Server{config: cfg}
	if host != nil {
		s.engineHost = host
	}
	return s
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

func engineEntries(t *testing.T, s *Server) []config.AgentConfig {
	t.Helper()
	cfg, err := config.Reload(s.settingsFilePath())
	if err != nil {
		t.Fatalf("config no longer reads: %v", err)
	}
	return cfg.Agents
}

// Adding an engine used to mean writing a sidecar and showing a restart banner.
// It starts the engine now, and the answer says so -- a UI that still keyed on
// restart_required would otherwise ask for a restart nothing needs.
func TestAddingAnEngineStartsItInsteadOfAskingForARestart(t *testing.T) {
	host := &fakeEngineHost{}
	s := engineTestServer(t, host)

	w := postEngine(t, s, `{"id":"vpn7","role":"hoard","bind_interface":"wg7"}`)
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
	ags := engineEntries(t, s)
	if len(ags) != 1 || ags[0].Name != "local-vpn7" || ags[0].EngineID != "vpn7" || ags[0].Role != "hoard" {
		t.Fatalf("[[agent]] entries = %+v, want one local-vpn7", ags)
	}
	if _, ok := ags[0].Session["listen_port"]; !ok {
		t.Errorf("the entry carries no listen_port: %+v", ags[0].Session)
	}
}

// The entry holds what is true of this engine and nothing else. The sidecar it
// replaces froze a copy of the whole primary config at creation time, which
// went stale the moment anything changed -- an engine announcing through last
// month's tunnel while every page reported green.
func TestTheEntryHoldsOnlyTheEnginesOwnKeys(t *testing.T) {
	s := engineTestServer(t, &fakeEngineHost{})
	if w := postEngine(t, s, `{"id":"vpn7","role":"hoard","bind_interface":"wg7"}`); w.Code != http.StatusOK {
		t.Fatalf("add: %d (%s)", w.Code, w.Body.String())
	}
	ags := engineEntries(t, s)
	if len(ags) != 1 {
		t.Fatalf("%d entries, want 1", len(ags))
	}
	for _, unwanted := range []string{"max_connections", "peer_timeout", "socks5_outbound_host"} {
		if _, present := ags[0].Session[unwanted]; present {
			t.Errorf("the entry copied %q from the profile instead of inheriting it", unwanted)
		}
	}
	// And it still RUNS the profile's values, which is the other half.
	cfg, _ := config.Reload(s.settingsFilePath())
	engs, err := cfg.ResolveEngines()
	if err != nil {
		t.Fatal(err)
	}
	for _, e := range engs {
		if e.ID != "vpn7" {
			continue
		}
		if e.MaxConnections != 12000 {
			t.Errorf("max_connections = %d, want the hoard profile's 12000", e.MaxConnections)
		}
		if e.BindInterface != "wg7" {
			t.Errorf("bind_interface = %q, want the engine's own wg7", e.BindInterface)
		}
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
	if ags := engineEntries(t, s); len(ags) != 0 {
		t.Errorf("the config holds %+v after a failed start, want nothing", ags)
	}
}

// A node with no engine host -- front-only -- still has to answer the old way,
// or the UI would report an engine as running on a node that runs none.
func TestWithoutAnEngineHostTheAnswerStillAsksForARestart(t *testing.T) {
	s := engineTestServer(t, nil)

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
	if ags := engineEntries(t, s); len(ags) != 0 {
		t.Errorf("the config still holds %+v", ags)
	}
}

// The list is what RUNS. Answering from the config file showed an engine that
// failed to start and hid one a restart had picked up from a hand-written
// entry -- both of which read as "everything is fine".
func TestTheEngineListReportsWhatIsRunning(t *testing.T) {
	host := &fakeEngineHost{running: []RunningEngine{{ID: "byhand", Role: "race", ListenPort: 26000}}}
	s := engineTestServer(t, host)

	r := gin.New()
	r.GET("/engines", s.handleEnginesGet)
	w := httptest.NewRecorder()
	r.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/engines", nil))

	var got []map[string]any
	if err := json.Unmarshal(w.Body.Bytes(), &got); err != nil {
		t.Fatalf("body: %v (%s)", err, w.Body.String())
	}
	if len(got) != 1 || got[0]["id"] != "byhand" || got[0]["agent"] != "local-byhand" {
		t.Fatalf("list = %v, want the running engine", got)
	}
}

type fakeStartErr struct{}

func (fakeStartErr) Error() string { return "listen port already taken" }

var errFakeStart = fakeStartErr{}
