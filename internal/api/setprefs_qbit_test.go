package api

import (
	"errors"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/gin-gonic/gin"

	"github.com/Kheopsian/hydra/internal/config"
)

// fakeRaceEngine records the port it was asked to bind. It leans on
// emptyRaceEngine for the rest of the interface — only the listen-port path
// matters here.
type fakeRaceEngine struct {
	emptyRaceEngine
	gotPort int
	live    int
	err     error
}

func (f *fakeRaceEngine) SetDialLimits(*float64, *int) error { return nil }

func (f *fakeRaceEngine) SetListenPort(port int) error {
	if f.err != nil {
		return f.err
	}
	f.gotPort = port
	f.live = port
	return nil
}

func (f *fakeRaceEngine) ListenPort() int { return f.live }

func setPrefsServer(t *testing.T, eng RaceEngine) (*Server, *gin.Engine) {
	t.Helper()
	gin.SetMode(gin.TestMode)

	dir := t.TempDir()
	tomlPath := filepath.Join(dir, "default.toml")
	if err := os.WriteFile(tomlPath, []byte("[race]\nlisten_port = 16171\n"), 0644); err != nil {
		t.Fatal(err)
	}
	cfg := &config.HydraConfig{SourcePath: tomlPath}
	cfg.Daemon.DataDir = dir
	s := &Server{config: cfg, raceEngine: eng}

	r := gin.New()
	r.POST("/setPreferences", s.qbitSetPreferences)
	return s, r
}

// tomlListenPort reads back what landed in the [race] section on disk. The
// Config tab renders this file, so it is what the user ends up seeing.
func tomlListenPort(t *testing.T, s *Server) string {
	t.Helper()
	data, err := os.ReadFile(s.settingsFilePath())
	if err != nil {
		t.Fatalf("read config: %v", err)
	}
	for _, ln := range strings.Split(string(data), "\n") {
		if strings.HasPrefix(strings.TrimSpace(ln), "listen_port") {
			return strings.TrimSpace(ln)
		}
	}
	return ""
}

func postPrefs(r *gin.Engine, jsonField string) *httptest.ResponseRecorder {
	form := url.Values{"json": {jsonField}}
	req := httptest.NewRequest(http.MethodPost, "/setPreferences", strings.NewReader(form.Encode()))
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	w := httptest.NewRecorder()
	r.ServeHTTP(w, req)
	return w
}

// The whole point of the endpoint: a VPN port-forward script pushes the
// rotated port the way every qBittorrent client does, and the engine rebinds.
func TestQbitSetPreferencesAppliesListenPort(t *testing.T) {
	eng := &fakeRaceEngine{}
	s, r := setPrefsServer(t, eng)

	if w := postPrefs(r, `{"listen_port":51413}`); w.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200 (body %q)", w.Code, w.Body.String())
	}
	if eng.gotPort != 51413 {
		t.Errorf("engine got port %d, want 51413", eng.gotPort)
	}
	// Written into the TOML itself: the file is what boot reads and what the
	// Config tab shows, so the port survives a restart and no screen
	// contradicts another.
	if got := tomlListenPort(t, s); got != "listen_port = 51413" {
		t.Errorf("config on disk has %q, want %q", got, "listen_port = 51413")
	}
}

// Clients that send the port as a string are common; rejecting them would be a
// pointless incompatibility.
func TestQbitSetPreferencesAcceptsStringPort(t *testing.T) {
	eng := &fakeRaceEngine{}
	_, r := setPrefsServer(t, eng)

	if w := postPrefs(r, `{"listen_port":"51413"}`); w.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200 (body %q)", w.Code, w.Body.String())
	}
	if eng.gotPort != 51413 {
		t.Errorf("engine got port %d, want 51413", eng.gotPort)
	}
}

// A full settings blob must not error over fields Hydra does not model —
// qBittorrent ignores unsupported preferences rather than rejecting the write.
func TestQbitSetPreferencesIgnoresUnknownKeys(t *testing.T) {
	eng := &fakeRaceEngine{}
	_, r := setPrefsServer(t, eng)

	w := postPrefs(r, `{"dht":true,"locale":"fr","max_connec":500}`)
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200 (body %q)", w.Code, w.Body.String())
	}
	if eng.gotPort != 0 {
		t.Errorf("no listen_port was sent, but the engine rebound to %d", eng.gotPort)
	}
}

// A failed rebind must surface. Answering 200 here is what left a node sitting
// on a port nothing forwards, with no signal to the script that pushed it.
func TestQbitSetPreferencesReportsRebindFailure(t *testing.T) {
	eng := &fakeRaceEngine{err: errors.New("engine down")}
	_, r := setPrefsServer(t, eng)

	if w := postPrefs(r, `{"listen_port":51413}`); w.Code != http.StatusInternalServerError {
		t.Fatalf("status = %d, want 500 (body %q)", w.Code, w.Body.String())
	}
}

func TestQbitSetPreferencesRejectsBadPort(t *testing.T) {
	for _, body := range []string{
		`{"listen_port":0}`,
		`{"listen_port":65536}`,
		`{"listen_port":"nope"}`,
		`{"listen_port":true}`,
	} {
		eng := &fakeRaceEngine{}
		_, r := setPrefsServer(t, eng)

		if w := postPrefs(r, body); w.Code != http.StatusBadRequest {
			t.Errorf("%s: status = %d, want 400", body, w.Code)
		}
		if eng.gotPort != 0 {
			t.Errorf("%s: engine rebound to %d on a rejected port", body, eng.gotPort)
		}
	}
}

// Some clients POST the preferences as a raw JSON body instead of the `json`
// form field.
func TestQbitSetPreferencesAcceptsRawJSONBody(t *testing.T) {
	eng := &fakeRaceEngine{}
	_, r := setPrefsServer(t, eng)

	req := httptest.NewRequest(http.MethodPost, "/setPreferences", strings.NewReader(`{"listen_port":51413}`))
	req.Header.Set("Content-Type", "application/json")
	w := httptest.NewRecorder()
	r.ServeHTTP(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200 (body %q)", w.Code, w.Body.String())
	}
	if eng.gotPort != 51413 {
		t.Errorf("engine got port %d, want 51413", eng.gotPort)
	}
}
