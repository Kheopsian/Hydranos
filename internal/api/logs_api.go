package api

import (
	"encoding/json"
	"fmt"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/Kheopsian/hydra/internal/logs"
	"github.com/gin-gonic/gin"
)

// handleLogs returns recent entries from the in-process log hub, filtered by
// ?since=<dur> (e.g. 15m), ?source=, ?level=, ?q=, ?limit=.
func (s *Server) handleLogs(c *gin.Context) {
	var since time.Time
	if d := c.Query("since"); d != "" {
		if dur, err := time.ParseDuration(d); err == nil && dur > 0 {
			since = time.Now().Add(-dur)
		}
	}
	limit := 2000
	if l := c.Query("limit"); l != "" {
		if n, err := strconv.Atoi(l); err == nil && n > 0 {
			limit = n
		}
	}
	entries := logs.Default.Query(since, c.Query("source"), c.Query("level"), c.Query("q"), limit)
	c.JSON(http.StatusOK, gin.H{"entries": entries})
}

// handleLogsStream is a Server-Sent Events live tail of new log entries.
func (s *Server) handleLogsStream(c *gin.Context) {
	flusher, ok := c.Writer.(http.Flusher)
	if !ok {
		c.String(http.StatusInternalServerError, "streaming unsupported")
		return
	}
	c.Writer.Header().Set("Content-Type", "text/event-stream")
	c.Writer.Header().Set("Cache-Control", "no-cache")
	c.Writer.Header().Set("Connection", "keep-alive")

	src := c.Query("source")
	lvl := c.Query("level")
	q := strings.ToLower(c.Query("q"))

	ch, cancel := logs.Default.Subscribe()
	defer cancel()
	flusher.Flush()

	ctx := c.Request.Context()
	for {
		select {
		case <-ctx.Done():
			return
		case e, ok := <-ch:
			if !ok {
				return
			}
			if src != "" && e.Source != src {
				continue
			}
			if lvl != "" && !logs.LevelAtLeast(e.Level, lvl) {
				continue
			}
			if q != "" && !strings.Contains(strings.ToLower(e.Msg), q) {
				continue
			}
			b, _ := json.Marshal(e)
			fmt.Fprintf(c.Writer, "data: %s\n\n", b)
			flusher.Flush()
		}
	}
}
