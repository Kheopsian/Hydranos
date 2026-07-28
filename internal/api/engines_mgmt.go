package api

import (
	"log/slog"
	"net/http"
	"os"
	"time"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/gin-gonic/gin"
)

// UI-managed extra local engines (Option A sharding from the Agents menu). The
// base [race]/[hoard] are fixed; these are additive shards persisted to
// engines.json and spawned on the next restart.

func (s *Server) handleEnginesGet(c *gin.Context) {
	engs, err := config.LoadExtraEngines(s.config.Daemon.DataDir)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	out := make([]gin.H, 0, len(engs))
	for _, e := range engs {
		out = append(out, gin.H{"id": e.ID, "role": e.Role, "listen_port": e.ListenPort})
	}
	c.JSON(http.StatusOK, out)
}

type engineAddReq struct {
	ID         string `json:"id"`
	Role       string `json:"role"`
	ListenPort int    `json:"listen_port"`
}

// nextFreePort returns a listen port above every engine currently known.
func (s *Server) nextFreePort() int {
	mx := 16172
	full, _ := s.config.ResolveEngines()
	extras, _ := config.LoadExtraEngines(s.config.Daemon.DataDir)
	for _, e := range append(full, extras...) {
		if e.ListenPort > mx {
			mx = e.ListenPort
		}
	}
	return mx + 1
}

func (s *Server) handleEnginesPost(c *gin.Context) {
	var req engineAddReq
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	if req.ID == "" || (req.Role != "race" && req.Role != "hoard") {
		c.JSON(http.StatusBadRequest, gin.H{"error": `id required; role must be "race" or "hoard"`})
		return
	}
	dataDir := s.config.Daemon.DataDir
	extras, _ := config.LoadExtraEngines(dataDir)

	// Inherit the same-role primary's tunables (crucially the SOCKS5 egress so
	// the shard announces through the VPS too), overriding id/port. proxy_v2
	// inbound is disabled for a shard (MVP: the relay only forwards the primary).
	base := s.config.Hoard
	if req.Role == "race" {
		base = s.config.Race
	}
	base.CustomChoking = nil
	base.ListenPortProxyV2 = 0
	if req.ListenPort > 0 {
		base.ListenPort = req.ListenPort
	} else {
		base.ListenPort = s.nextFreePort()
	}
	ec := config.EngineConfig{ID: req.ID, Role: req.Role, SessionConfig: base}
	extras = append(extras, ec)

	full, _ := s.config.ResolveEngines()
	if verr := config.ValidateEngines(append(full, extras...)); verr != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": verr.Error()})
		return
	}
	if err := config.SaveExtraEngines(dataDir, extras); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, gin.H{"ok": true, "restart_required": true,
		"engine": gin.H{"id": ec.ID, "role": ec.Role, "listen_port": ec.ListenPort}})
}

func (s *Server) handleEnginesDelete(c *gin.Context) {
	id := c.Param("id")
	dataDir := s.config.Daemon.DataDir
	extras, _ := config.LoadExtraEngines(dataDir)
	out := make([]config.EngineConfig, 0, len(extras))
	found := false
	for _, e := range extras {
		if e.ID == id {
			found = true
			continue
		}
		out = append(out, e)
	}
	if !found {
		c.JSON(http.StatusNotFound, gin.H{"error": "unknown engine"})
		return
	}
	if err := config.SaveExtraEngines(dataDir, out); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, gin.H{"ok": true, "restart_required": true})
}

// handleRestart exits the process; the container's restart policy brings it back
// with the new engine set. Rollback = delete the engine + restart again.
func (s *Server) handleRestart(c *gin.Context) {
	c.JSON(http.StatusOK, gin.H{"ok": true, "restarting": true})
	go func() {
		time.Sleep(500 * time.Millisecond)
		slog.Info("restart requested via API — exiting for container restart")
		os.Exit(0)
	}()
}
