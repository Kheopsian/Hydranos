package api

import (
	"fmt"
	"log/slog"
	"net/http"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/gin-gonic/gin"
)

// Extra engines, managed from the Agents menu. The base [race]/[hoard] are the
// fleet profiles; each extra engine is an [[agent]] entry with no addr -- one
// agent, one engine, started by this process -- carrying only what is true of
// itself and inheriting the rest of its role's profile.

// extraEngineAgentName is the agent an extra engine is registered under. The
// prefix is not decoration: a category targets an AGENT, so the name is the
// address, and it has to be the same one everywhere it is written.
func extraEngineAgentName(id string) string { return LocalAgentNameFor(id) }

func (s *Server) handleEnginesGet(c *gin.Context) {
	// What is running, not what is written. A file listing showed an engine
	// that failed to start and hid one added by hand to the config.
	if s.engineHost != nil {
		engs := s.engineHost.Engines()
		out := make([]gin.H, 0, len(engs))
		for _, e := range engs {
			out = append(out, gin.H{"id": e.ID, "role": e.Role,
				"listen_port": e.ListenPort, "bind_interface": e.BindInterface,
				"agent": extraEngineAgentName(e.ID)})
		}
		c.JSON(http.StatusOK, out)
		return
	}
	c.JSON(http.StatusOK, []gin.H{})
}

type engineAddReq struct {
	ID            string `json:"id"`
	Role          string `json:"role"`
	ListenPort    int    `json:"listen_port"`
	BindInterface string `json:"bind_interface"`
}

// nextFreePort returns a listen port above every engine currently known.
func (s *Server) nextFreePort() int {
	mx := 16172
	full, _ := s.liveConfig().ResolveEngines()
	for _, e := range full {
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
	req.ID = strings.TrimSpace(req.ID)
	if req.ID == "" || (req.Role != "race" && req.Role != "hoard") {
		c.JSON(http.StatusBadRequest, gin.H{"error": `id required; role must be "race" or "hoard"`})
		return
	}
	// The id, not the agent name it produces: every extra engine's agent name
	// starts with "local-" by construction, so checking that would reject them
	// all. What must be refused is an id that collides with the two primaries
	// or with the bare alias.
	if isReservedAgentName(req.ID) || req.ID == "race" || req.ID == "hoard" {
		c.JSON(http.StatusBadRequest, gin.H{"error": `"local", "race" and "hoard" are reserved engine ids`})
		return
	}
	cfg := s.liveConfig()
	if req.ListenPort <= 0 {
		req.ListenPort = s.nextFreePort()
	}

	// The engine's OWN keys, and only those. Everything else comes from the
	// [race]/[hoard] profile for its role, which is where a fleet-wide change
	// is made once. The sidecar this replaces held a frozen copy of the whole
	// primary config, taken at creation, and went stale the moment anything
	// changed -- a shard announcing through last month's tunnel while every
	// page reported green.
	ag := config.AgentConfig{
		Name: extraEngineAgentName(req.ID), Role: req.Role, EngineID: req.ID,
		Session: map[string]interface{}{"listen_port": int64(req.ListenPort)},
	}
	if bi := strings.TrimSpace(req.BindInterface); bi != "" {
		ag.Session["bind_interface"] = bi
	}
	sess, serr := cfg.LocalEngineSession(&ag)
	if serr != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": serr.Error()})
		return
	}
	ec := config.EngineConfig{ID: req.ID, Role: req.Role, SessionConfig: sess}

	full, _ := cfg.ResolveEngines()
	if verr := config.ValidateEngines(append(full, ec)); verr != nil {
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
	if err := s.writeEngineEntry(ag); err != nil {
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
		"agent": ag.Name,
		"engine": gin.H{"id": ec.ID, "role": ec.Role, "listen_port": ec.ListenPort,
			"bind_interface": ec.BindInterface}})
}

// writeEngineEntry records one extra engine as an [[agent]] entry, in place.
func (s *Server) writeEngineEntry(ag config.AgentConfig) error {
	kv := [][2]string{
		{"role", strconv.Quote(ag.Role)},
		{"engine_id", strconv.Quote(ag.EngineID)},
	}
	for _, k := range []string{"listen_port", "bind_interface"} {
		v, ok := ag.Session[k]
		if !ok {
			continue
		}
		switch tv := v.(type) {
		case int64:
			kv = append(kv, [2]string{"session." + k, strconv.FormatInt(tv, 10)})
		case string:
			kv = append(kv, [2]string{"session." + k, strconv.Quote(tv)})
		default:
			return fmt.Errorf("engine key %q has an unexpected type %T", k, v)
		}
	}
	return s.editConfigFile(func(doc string) (string, error) {
		return config.SetTOMLArrayTable(doc, "agent", "name", ag.Name, kv)
	})
}

func (s *Server) handleEnginesDelete(c *gin.Context) {
	id := c.Param("id")
	name := extraEngineAgentName(id)
	known := false
	if s.engineHost != nil {
		for _, e := range s.engineHost.Engines() {
			if e.ID == id {
				known = true
				break
			}
		}
	}
	if !known && s.liveConfig().AgentByName(name) == nil {
		c.JSON(http.StatusNotFound, gin.H{"error": "unknown engine"})
		return
	}
	// Persist first here, the opposite of the add, and for the same reason: if
	// the config write fails after the engine is down, the engine comes back at
	// the next restart and the delete silently undoes itself.
	if err := s.editConfigFile(func(doc string) (string, error) {
		return config.DeleteTOMLArrayTable(doc, "agent", "name", name), nil
	}); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	stopped := false
	if s.engineHost != nil && known {
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
