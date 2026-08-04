package api

import (
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

func (s *Server) pauseOne(c *gin.Context, paused, race bool) {
	hash := c.Param("info_hash")
	var err error
	if race {
		if s.raceEngine == nil {
			c.JSON(http.StatusServiceUnavailable, gin.H{"error": "race engine not available"})
			return
		}
		err = s.raceEngine.SetUserPaused(hash, paused)
	} else {
		if s.hoardEngine == nil {
			c.JSON(http.StatusServiceUnavailable, gin.H{"error": "hoard engine not available"})
			return
		}
		err = s.hoardEngine.SetUserPaused(hash, paused)
	}
	if err != nil {
		c.JSON(http.StatusNotFound, gin.H{"error": err.Error()})
		return
	}
	persistPaused([]string{hash}, paused)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "info_hash": hash, "paused": paused})
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
	applied := make([]string, 0, len(body.Hashes))
	for _, h := range body.Hashes {
		var err error
		if race {
			if s.raceEngine == nil {
				break
			}
			err = s.raceEngine.SetUserPaused(h, body.Paused)
		} else {
			if s.hoardEngine == nil {
				break
			}
			err = s.hoardEngine.SetUserPaused(h, body.Paused)
		}
		if err == nil {
			applied = append(applied, h)
		}
	}
	persistPaused(applied, body.Paused)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "applied": len(applied), "paused": body.Paused})
}
