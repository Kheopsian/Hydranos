package api

import (
	"net/http"

	"github.com/gin-gonic/gin"
)

// handleHealthAnomalies returns the latest health-invariant report: torrents
// that violate a BitTorrent conservation law (re-download, fake-seed, starved
// leech, missing files) plus cumulative counters that survive restarts.
func (s *Server) handleHealthAnomalies(c *gin.Context) {
	if s.healthReporter == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "health scanner not wired"})
		return
	}
	c.JSON(http.StatusOK, s.healthReporter.Snapshot())
}
