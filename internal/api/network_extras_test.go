package api

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
)

// Saving the Network tab with extra engines deadlocked the handler against
// itself: it holds configWriteMu for the whole read-rewrite-validate cycle, and
// the per-engine keys were written through editConfigFile, which takes that
// same plain Mutex. The request never answered and every later config write
// queued behind it forever.
//
// Time-boxed rather than simply called, because the failure mode is a hang: a
// plain call would take the whole test binary down with it.
func TestSavingTheNetworkTabWithExtraEnginesAnswers(t *testing.T) {
	host := &fakeEngineHost{running: []RunningEngine{{ID: "vpn7", Role: "hoard", ListenPort: 26991}}}
	s := engineTestServer(t, host)

	r := gin.New()
	r.POST("/network/mode", s.handleNetworkModePost)
	body := `{"mode":"direct","fields":{"race_listen_port":16171,"hoard_listen_port":16172},
		"extra_engines":[{"id":"vpn7","role":"hoard","bind_interface":"lo","listen_port":26991}]}`

	done := make(chan *httptest.ResponseRecorder, 1)
	go func() {
		w := httptest.NewRecorder()
		req := httptest.NewRequest(http.MethodPost, "/network/mode", strings.NewReader(body))
		req.Header.Set("Content-Type", "application/json")
		r.ServeHTTP(w, req)
		done <- w
	}()

	select {
	case w := <-done:
		if w.Code != http.StatusOK {
			t.Fatalf("status = %d, want 200 (%s)", w.Code, w.Body.String())
		}
		// The engine's own interface must have landed in its entry, or the save
		// answered without doing the thing it was asked for.
		for _, ag := range engineEntries(t, s) {
			if ag.Name != "local-vpn7" {
				continue
			}
			if iface, _ := ag.Session["bind_interface"].(string); iface != "lo" {
				t.Errorf("entry bind_interface = %q, want lo", iface)
			}
			return
		}
		t.Errorf("no local-vpn7 entry was written")
	case <-time.After(10 * time.Second):
		t.Fatal("the save never answered: the handler is deadlocked on configWriteMu")
	}
}

// A port left where it was must not put the restart banner back up. The banner
// costs every peer connection on the node when it is followed, so it has to
// mean something.
func TestSavingWithoutChangingAPortAsksForNoRestart(t *testing.T) {
	host := &fakeEngineHost{running: []RunningEngine{{ID: "vpn7", Role: "hoard", ListenPort: 26991}}}
	s := engineTestServer(t, host)

	r := gin.New()
	r.POST("/network/mode", s.handleNetworkModePost)
	w := httptest.NewRecorder()
	req := httptest.NewRequest(http.MethodPost, "/network/mode", strings.NewReader(
		`{"mode":"direct","fields":{"race_listen_port":16171,"hoard_listen_port":16172,"race_bind_interface":"lo"},
		  "extra_engines":[{"id":"vpn7","role":"hoard","bind_interface":"lo","listen_port":26991}]}`))
	req.Header.Set("Content-Type", "application/json")
	r.ServeHTTP(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("status = %d (%s)", w.Code, w.Body.String())
	}
	if strings.Contains(w.Body.String(), `"restart_required":true`) {
		t.Errorf("asked for a restart with every listen port unchanged: %s", w.Body.String())
	}
}

// Moving a port is the one change a running engine cannot take from a pushed
// config, so it is the one that still needs a restart.
func TestMovingAListenPortAsksForARestart(t *testing.T) {
	s := engineTestServer(t, &fakeEngineHost{})

	r := gin.New()
	r.POST("/network/mode", s.handleNetworkModePost)
	w := httptest.NewRecorder()
	req := httptest.NewRequest(http.MethodPost, "/network/mode", strings.NewReader(
		`{"mode":"direct","fields":{"race_listen_port":16171,"hoard_listen_port":26000}}`))
	req.Header.Set("Content-Type", "application/json")
	r.ServeHTTP(w, req)

	if !strings.Contains(w.Body.String(), `"restart_required":true`) {
		t.Errorf("a moved listen port did not ask for a restart: %s", w.Body.String())
	}
}
