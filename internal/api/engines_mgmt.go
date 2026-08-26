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
// base [race]/[hoard] are fixed; these are additive, persisted to engines.json
// and -- since the engine host owns them -- started and stopped on the spot.

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

	full, _ := s.config.ResolveEngines()
	if verr := config.ValidateEngines(append(append(full, extras...), ec)); verr != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": verr.Error()})
		return
	}

	// Start BEFORE persisting. An engine that cannot come up -- a port already
	// taken by something outside Hydra, a data_dir that will not hold a store --
	// would otherwise be written down and fail identically at every boot from
	// here on, with the failure buried in the startup log instead of being the
	// answer to this request.
	started := false
	if s.engineHost != nil {
		if err := s.engineHost.AddEngine(ec); err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
			return
		}
		started = true
	}
	if err := config.SaveExtraEngines(dataDir, append(extras, ec)); err != nil {
		if started {
			// It runs but nothing recorded it: it would vanish at the next
			// restart, which is a worse state than the add simply failing.
			if rerr := s.engineHost.RemoveEngine(ec.ID); rerr != nil {
				slog.Error("engine add: rolling back the started engine failed",
					"engine", ec.ID, "error", rerr)
			}
		}
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, gin.H{"ok": true, "restart_required": !started, "started": started,
		"agent":  LocalAgentNameFor(ec.ID),
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
	// Persist first here, the opposite of the add, and for the same reason: if
	// the file write fails after the engine is down, the engine comes back at
	// the next restart and the delete silently undoes itself.
	if err := config.SaveExtraEngines(dataDir, out); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	stopped := false
	if s.engineHost != nil {
		if err := s.engineHost.RemoveEngine(id); err != nil {
			// The config no longer has it, so a restart finishes the job; say
			// so rather than reporting a failure for a delete that did happen.
			slog.Warn("engine delete: removed from the config but not stopped",
				"engine", id, "error", err)
		} else {
			stopped = true
		}
	}
	c.JSON(http.StatusOK, gin.H{"ok": true, "restart_required": !stopped, "stopped": stopped})
}

// handleRestart exits the process; the container's restart policy brings it back
// with the new engine set. Rollback = delete the engine + restart again.
func (s *Server) handleRestart(c *gin.Context) {
	c.JSON(http.StatusOK, gin.H{"ok": true, "restarting": true})
	go func() {
		time.Sleep(500 * time.Millisecond)
		slog.Info("restart requested via API, exiting for container restart")
		os.Exit(0)
	}()
}
