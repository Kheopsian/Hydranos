package api

import (
	"net/http"

	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/gin-gonic/gin"
)

// Startup pause.
//
// Deliberately NOT part of routes_pause.go: everything there writes the user's
// per-torrent intent to the store. This writes nothing. The startup pause is a
// process-level hold on outbound traffic (announces and peer dials), armed by
// `start_paused` in the engine config so a user behind a VPN can adjust their
// limits before a large library's boot-time wave hits the tunnel. Releasing it
// must leave every torrent's own paused intent exactly as the user set it.

// handleGetStartupPause reports which engines are holding. The UI polls this
// to decide whether to show the banner -- a held engine that looked idle with
// no explanation would read as a broken install.
func (s *Server) handleGetStartupPause(c *gin.Context) {
	held := engine.HeldStartupScopes()
	c.JSON(http.StatusOK, gin.H{
		"held":    held,
		"holding": len(held) > 0,
	})
}

// handleReleaseStartupPause lifts the hold on every engine at once. There is
// no per-engine release: a user staring at a stopped Hydra wants it running,
// and a half-released state is one more thing to explain.
//
// Idempotent: releasing when nothing is held is a 200 with an empty list, so
// a double-click or a stale banner cannot produce an error the user has to
// reason about.
func (s *Server) handleReleaseStartupPause(c *gin.Context) {
	released := []string{}
	if s.startupPauseRelease != nil {
		released = s.startupPauseRelease()
	}
	c.JSON(http.StatusOK, gin.H{
		"status":   "ok",
		"released": released,
		"holding":  len(engine.HeldStartupScopes()) > 0,
	})
}
