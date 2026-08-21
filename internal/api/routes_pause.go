package api

import (
	"errors"
	"fmt"
	"log/slog"
	"net/http"

	"github.com/Kheopsian/hydra/internal/store"
	"github.com/gin-gonic/gin"
)

// Manual pause.
//
// Every route here writes the user's *intent* and then acts on it. The intent
// is what gets persisted, on the torrent's own row; the schedulers read it and
// never write it. See internal/engine/pause.go for why the two are separate.

// errNoEngine means this node has no engine of that kind at all — distinct
// from "no engine here holds it", which routes to an agent instead.
var errNoEngine = errors.New("engine not available")

func engineName(race bool) string {
	if race {
		return "race"
	}
	return "hoard"
}

type pauseBody struct {
	Hashes []string `json:"hashes"`
	Paused bool     `json:"paused"`
}

// persistPaused records the intent for a batch of torrents in one transaction.
// A failure here is loud: the engine has already acted, so a silent miss would
// mean the torrent comes back on the next restart with no trace of why.
func persistPaused(hashes []string, paused bool) {
	st := durable()
	if st == nil || len(hashes) == 0 {
		return
	}
	if _, err := st.SetPausedMany(hashes, paused); err != nil {
		slog.Error("pause: persisting intent failed",
			"count", len(hashes), "paused", paused, "err", err)
	}
}

// persistPausedSession records the intent for a whole engine in one statement,
// which is what makes pause-all affordable at 100k torrents.
func persistPausedSession(sess store.Session, paused bool) {
	st := durable()
	if st == nil {
		return
	}
	if _, err := st.SetPausedSession(sess, paused); err != nil {
		slog.Error("pause: persisting session intent failed",
			"session", string(sess), "paused", paused, "err", err)
	}
}

func (s *Server) handleHoardPause(c *gin.Context)  { s.pauseOne(c, true, false) }
func (s *Server) handleHoardResume(c *gin.Context) { s.pauseOne(c, false, false) }
func (s *Server) handleRacePause(c *gin.Context)   { s.pauseOne(c, true, true) }
func (s *Server) handleRaceResume(c *gin.Context)  { s.pauseOne(c, false, true) }

// setPausedOne writes the intent wherever the torrent actually lives and
// reports who took it ("local" or an agent name).
//
// A torrent on an agent is paused through that agent's OWN engine, not with a
// thin stop: the agent records the intent, so its slot manager stops handing
// the torrent a download slot again on its next pass. Without this, stop and
// start on an agent row were applied to the local engine, which has never
// heard of the hash -- 404 on a monolith, and a silent no-op on a front-only
// node whose local engine is a stub that answers "fine" to everything.
func (s *Server) setPausedOne(hash string, paused, race bool) (string, error) {
	var local interface {
		HasTorrent(string) bool
		SetUserPaused(string, bool) error
	}
	if race {
		if s.raceEngine != nil {
			local = s.raceEngine
		}
	} else if s.hoardEngine != nil {
		local = s.hoardEngine
	}
	if local != nil && local.HasTorrent(hash) {
		return "local", local.SetUserPaused(hash, paused)
	}
	action := "resume"
	if paused {
		action = "pause"
	}
	// Hoard rows are already cached by the row poller, so a whole selection
	// resolves without a round trip. Anything it does not know (a race
	// torrent, a hash no listing carried) falls back to probing the agents.
	if !race {
		if name, engineID, ok := s.agentHoardOwner(hash); ok {
			if ra := s.remoteAgentByName(name); ra != nil {
				if cl := ra.anyClient(); cl != nil {
					return name, cl.ActionRouted(engineID, action, hash, false, "", "")
				}
			}
		}
	}
	if ra, mode, ok := s.findRemoteOwner(hash); ok {
		return ra.name, ra.anyClient().ActionRouted(mode, action, hash, false, "", "")
	}
	if local == nil {
		return "", errNoEngine
	}
	if s.frontOnly {
		// The "local engine" here is the front-only stub: it answers nil to
		// everything, so falling through would report ok having done nothing —
		// the exact silent no-op this path exists to remove.
		return "", fmt.Errorf("no engine or agent holds %s", hash)
	}
	// Claimed by nobody: let the local engine raise the not-found it always did.
	return "local", local.SetUserPaused(hash, paused)
}

func (s *Server) pauseOne(c *gin.Context, paused, race bool) {
	hash := c.Param("info_hash")
	agent, err := s.setPausedOne(hash, paused, race)
	switch {
	case errors.Is(err, errNoEngine):
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": engineName(race) + " engine not available"})
		return
	case err != nil && agent != "" && agent != "local":
		c.JSON(http.StatusBadGateway, gin.H{"error": err.Error(), "agent": agent})
		return
	case err != nil:
		c.JSON(http.StatusNotFound, gin.H{"error": err.Error()})
		return
	}
	// Only a local torrent has a row in this node's store; an agent persists
	// the intent on its own side.
	if agent == "local" {
		persistPaused([]string{hash}, paused)
	}
	c.JSON(http.StatusOK, gin.H{"status": "ok", "info_hash": hash, "paused": paused, "agent": agent})
}

// handleHoardPauseBulk applies one intent to a selection — what the UI sends
// when several rows are highlighted.
func (s *Server) handleHoardPauseBulk(c *gin.Context) { s.pauseBulk(c, false) }
func (s *Server) handleRacePauseBulk(c *gin.Context)  { s.pauseBulk(c, true) }

func (s *Server) pauseBulk(c *gin.Context, race bool) {
	var body pauseBody
	if err := c.ShouldBindJSON(&body); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "expected {hashes: [...], paused: bool}"})
		return
	}
	applied, localApplied := 0, make([]string, 0, len(body.Hashes))
	for _, h := range body.Hashes {
		agent, err := s.setPausedOne(h, body.Paused, race)
		if errors.Is(err, errNoEngine) {
			break
		}
		if err != nil {
			continue
		}
		applied++
		if agent == "local" {
			localApplied = append(localApplied, h)
		}
	}
	persistPaused(localApplied, body.Paused)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "applied": applied, "paused": body.Paused})
}
