package api

import (
	"net/http"

	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/gin-gonic/gin"
)

// bulkBody is "do this to everything I have on screen".
//
// The filter travels instead of the hashes: at 100k torrents a "select all"
// would otherwise be a multi-megabyte request describing a set the daemon
// already holds. Exclusions travel too, because the real gesture is usually
// "all of them except these three".
//
// Hashes stay supported for small, explicit selections.
type bulkBody struct {
	Action  string               `json:"action"` // "stop" | "start"
	Filter  engine.TorrentFilter `json:"filter"`
	Exclude []string             `json:"exclude"`
	Hashes  []string             `json:"hashes"`
}

func (s *Server) handleHoardBulk(c *gin.Context) { s.bulkAction(c, false) }
func (s *Server) handleRaceBulk(c *gin.Context)  { s.bulkAction(c, true) }

// bulkAction applies stop/start to everything the filter matches.
//
// It answers with the count it matched, and the UI compares that against what
// it displayed: the filter is implemented twice (here and in the browser), so
// the only real risk is the two drifting apart, and a visible number is what
// turns that from a silent wrong-set into something somebody notices.
func (s *Server) bulkAction(c *gin.Context, race bool) {
	var body bulkBody
	if err := c.ShouldBindJSON(&body); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": `expected {action, filter, exclude, hashes}`})
		return
	}
	var stop bool
	switch body.Action {
	case "stop":
		stop = true
	case "start":
		stop = false
	default:
		c.JSON(http.StatusBadRequest, gin.H{"error": `action must be "stop" or "start"`})
		return
	}

	exclude := make(map[string]bool, len(body.Exclude))
	for _, h := range body.Exclude {
		exclude[h] = true
	}

	hashes := body.Hashes
	if len(hashes) == 0 {
		if race {
			if s.raceEngine == nil {
				c.JSON(http.StatusServiceUnavailable, gin.H{"error": "race engine not available"})
				return
			}
			hashes = s.raceEngine.MatchHashes(body.Filter, exclude)
		} else {
			if s.hoardEngine == nil {
				c.JSON(http.StatusServiceUnavailable, gin.H{"error": "hoard engine not available"})
				return
			}
			hashes = s.hoardEngine.MatchHashes(body.Filter, exclude)
		}
	} else if len(exclude) > 0 {
		kept := hashes[:0]
		for _, h := range hashes {
			if !exclude[h] {
				kept = append(kept, h)
			}
		}
		hashes = kept
	}

	matched := len(hashes)
	applied := 0
	// Only what stayed local has a row in this node's store; an agent persists
	// the intent of its own torrents itself.
	localApplied := make([]string, 0, matched)
	for _, h := range hashes {
		agent, err := s.setPausedOne(h, stop, race)
		if err != nil {
			continue
		}
		applied++
		if agent == "local" {
			localApplied = append(localApplied, h)
		}
	}
	persistPaused(localApplied, stop)

	c.JSON(http.StatusOK, gin.H{
		"status":  "ok",
		"action":  body.Action,
		"matched": matched,
		"applied": applied,
		"failed":  matched - applied,
	})
}
