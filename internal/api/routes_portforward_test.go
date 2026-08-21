package api

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/gin-gonic/gin"
)

// peerHoard reports an aggregate and records whether anyone asked for the
// listing. Building the listing to add up one integer is what this guards.
type peerHoard struct {
	HoardEngine
	peers     int
	listCalls int
}

func (h *peerHoard) GetAllStatus() map[string]interface{} {
	return map[string]interface{}{"active_peers": h.peers}
}

func (h *peerHoard) GetTorrentList() []map[string]interface{} {
	h.listCalls++
	return []map[string]interface{}{
		{"info_hash": "aaa", "num_peers": 3},
		{"info_hash": "bbb", "num_peers": 4},
	}
}

func (h *peerHoard) ListenPort() int { return 16172 }

// /api/port-forward is polled by the UI and is in the logger's SkipPaths, so
// the cost of answering it never showed up in the access log. It summed
// num_peers by materialising a map per torrent -- 196k of them in production,
// 17% of everything the process allocated -- for a total the engine already
// keeps.
func TestPortForwardStatusDoesNotBuildTheListing(t *testing.T) {
	gin.SetMode(gin.TestMode)

	h := &peerHoard{peers: 7}
	s := &Server{hoardEngine: h, config: &config.HydraConfig{}}
	r := gin.New()
	r.GET("/port-forward", s.handlePortForwardStatus)

	w := httptest.NewRecorder()
	r.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/port-forward", nil))
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200 (%s)", w.Code, w.Body.String())
	}

	var got map[string]interface{}
	if err := json.Unmarshal(w.Body.Bytes(), &got); err != nil {
		t.Fatalf("body: %v (%s)", err, w.Body.String())
	}
	if h.listCalls != 0 {
		t.Errorf("built the torrent listing %d time(s) to sum one integer", h.listCalls)
	}

	if p, _ := got["hoard_peers"].(float64); int(p) != 7 {
		t.Errorf("hoard_peers = %v, want 7 (the engine's aggregate)", got["hoard_peers"])
	}
}
