package api

import (
	"compress/gzip"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/gin-gonic/gin"
)

// gzFlusher flushes the optional gzip layer, then the socket, so live SSE
// frames still reach the client promptly under Content-Encoding: gzip.
type gzFlusher struct {
	gz   *gzip.Writer
	base http.Flusher
}

func (g gzFlusher) Flush() {
	if g.gz != nil {
		_ = g.gz.Flush()
	}
	g.base.Flush()
}

// handleSSE streams Typhon push events (stats_snapshot, torrent_added,
// torrent_removed, ...) straight to the browser over Server-Sent Events.
// Frontend consumes via `new EventSource('/api/events')`.
//
// Frame format: standard SSE `data: <json>\n\n`, same JSON shape as what
// the Go client receives from Typhon: `{"event": "<type>", "data": {...}}`.
// Heartbeat comment every 30s to keep reverse-proxies from dropping idle.
func (s *Server) handleSSE(c *gin.Context) {
	if s.hoardEngine == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "hoard engine not ready"})
		return
	}
	hub := s.hoardEngine.EventHub()
	if hub == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "event hub not available (engine doesn't support push)"})
		return
	}

	c.Writer.Header().Set("Content-Type", "text/event-stream")
	c.Writer.Header().Set("Cache-Control", "no-cache")
	c.Writer.Header().Set("Connection", "keep-alive")
	// Disable buffering on Nginx and similar reverse proxies.
	c.Writer.Header().Set("X-Accel-Buffering", "no")
	// gzip the whole stream when the client accepts it: the hydration payload
	// (~64 MB of repetitive JSON at 106k torrents) compresses ~6-8x, decisive
	// over slow links. EventSource inflates transparently.
	gzipOn := strings.Contains(c.GetHeader("Accept-Encoding"), "gzip")
	if gzipOn {
		c.Writer.Header().Set("Content-Encoding", "gzip")
	}
	c.Writer.WriteHeader(http.StatusOK)

	flusher, ok := c.Writer.(http.Flusher)
	if !ok {
		c.AbortWithStatus(http.StatusInternalServerError)
		return
	}
	var out io.Writer = c.Writer
	var gz *gzip.Writer
	if gzipOn {
		gz = gzip.NewWriter(c.Writer)
		out = gz
		defer gz.Close()
	}
	fl := gzFlusher{gz: gz, base: flusher}

	// Subscribe and guarantee cleanup on disconnect.
	id, ch := hub.Subscribe()
	defer hub.Unsubscribe(id)

	// Count real browsers, so work that only exists to feed them (polling the
	// agents, see agentrows.go) stops when the last tab closes.
	s.sseClients.Add(1)
	defer s.sseClients.Add(-1)

	// Initial comment tells the browser the stream is open (no data yet).
	fmt.Fprintf(out, ": connected\n\n")
	fl.Flush()

	// Emit a current stats snapshot immediately so the overview header paints
	// on connect/reconnect — decoupled from the row hydration below, which can
	// take a while at 100k torrents. Without this, returning to a backgrounded
	// tab (which closes+reopens the SSE) left the header frozen until the whole
	// list had re-streamed.
	s.emitSnapshot(out, fl)

	// Incremental reconnect (see reconnect.go): a client returning to a still-
	// open tab sends ?since=<server_ts it last saw>; reply with just the delta
	// (adds after the cursor + tombstoned removes) and keep its list. Fall back
	// to a full hydration when completeness isn't guaranteed (no cursor, cursor
	// too old, or the server restarted). streamHydration also serves first load.
	served := false
	if sinceStr := c.Query("since"); sinceStr != "" {
		if since, err := strconv.ParseFloat(sinceStr, 64); err == nil {
			served = s.streamDelta(out, fl, since)
		}
	}
	if !served {
		rcWriteFrame(out, map[string]interface{}{"event": "sync", "data": map[string]interface{}{"mode": "full"}})
		fl.Flush()
		s.streamHydration(out, fl)
	}

	hb := time.NewTicker(30 * time.Second)
	defer hb.Stop()

	ctx := c.Request.Context()
	for {
		select {
		case <-ctx.Done():
			return
		case data, open := <-ch:
			if !open {
				return
			}
			// SSE frame: every byte in `data` is a single line of JSON
			// (no \n inside since the engine emits one frame per newline).
			if _, err := fmt.Fprintf(out, "data: %s\n\n", data); err != nil {
				return
			}
			fl.Flush()
		case <-hb.C:
			if _, err := fmt.Fprintf(out, ": heartbeat\n\n"); err != nil {
				return
			}
			fl.Flush()
		}
	}
}

// emitSnapshot writes one status_snapshot + hoard_stats_snapshot frame using
// the same builders as the 1Hz pusher, so a freshly (re)connected client gets a
// correct overview header without waiting for the 1Hz tick or the row hydration.
func (s *Server) emitSnapshot(w interface{ Write([]byte) (int, error) }, flusher interface{ Flush() }) {
	frames := []struct {
		name string
		data interface{}
	}{
		{"status_snapshot", s.statusPayload()},
		{"hoard_stats_snapshot", s.hoardStatsPayload()},
	}
	for _, f := range frames {
		frame, err := json.Marshal(map[string]interface{}{"event": f.name, "data": f.data})
		if err != nil {
			continue
		}
		fmt.Fprintf(w, "data: %s\n\n", frame)
	}
	flusher.Flush()
}

// streamHydration emits the current torrent lists (hoard + race) as
// torrent_batch SSE events, chunked and ordered by added_time descending so the
// most recent torrents render first. done=true on the last chunk of a mode.
func (s *Server) streamHydration(w interface{ Write([]byte) (int, error) }, flusher interface{ Flush() }) {
	const chunk = 1000
	emit := func(mode string, rows []json.RawMessage, done bool) {
		frame, err := json.Marshal(map[string]interface{}{
			"event": "torrent_batch",
			"data":  map[string]interface{}{"mode": mode, "torrents": rows, "done": done},
		})
		if err != nil {
			return
		}
		fmt.Fprintf(w, "data: %s\n\n", frame)
		flusher.Flush()
	}
	stream := func(mode string, list []json.RawMessage, done bool) {
		if len(list) == 0 {
			if done {
				emit(mode, nil, true)
			}
			return
		}
		for i := 0; i < len(list); i += chunk {
			end := i + chunk
			if end > len(list) {
				end = len(list)
			}
			emit(mode, list[i:end], done && end == len(list))
		}
	}

	// A node's own engines are only part of the picture: torrents living on
	// registered agents belong in the same list, and this is the ONLY path that
	// fills it. /api/hoard/torrents has aggregated agents all along, but the
	// list stopped reading it when hydration moved to SSE, so agent torrents
	// silently vanished from the UI -- visible the moment a torrent is moved to
	// an agent and appears to disappear.
	//
	// Local rows go first and agent rows follow, so the bulk of the list paints
	// without waiting on the network, and `done` is only set on the very last
	// batch of a mode.
	agents := s.agentsSnapshot()
	agentRows := func(role string) []json.RawMessage {
		var rows []json.RawMessage
		// Hoard rides the cache the row pusher keeps (see agentrows.go): the
		// live pushes diff against what hydration painted, so both have to be
		// the same snapshot, and a browser connecting must not cost one
		// listing per agent on top of the poll.
		if role == "hoard" {
			for _, r := range s.agentHoardRows() {
				if b, err := json.Marshal(r); err == nil {
					rows = append(rows, b)
				}
			}
			return rows
		}
		for _, ra := range agents {
			for _, e := range ra.byRole(role) {
				// Bounded: a slow or dead agent must not hold up hydration for
				// everything else.
				lst, err := e.client.ListTorrentsTimeout(4 * time.Second)
				if err != nil || lst == nil {
					continue
				}
				cats, _ := e.client.TorrentCategories(e.id)
				for _, t := range lst.Torrents {
					if b, mErr := json.Marshal(ltStatusToRow(t, ra.name, cats)); mErr == nil {
						rows = append(rows, b)
					}
				}
			}
		}
		return rows
	}
	hasRole := func(role string) bool {
		for _, ra := range agents {
			if len(ra.byRole(role)) > 0 {
				return true
			}
		}
		return false
	}

	for _, m := range []struct {
		mode, role string
		local      func() []json.RawMessage
	}{
		{"hoard", "hoard", func() []json.RawMessage {
			if s.hoardEngine == nil {
				return nil
			}
			return s.hoardEngine.GetTorrentListJSON()
		}},
		{"race", "race", func() []json.RawMessage {
			if s.raceEngine == nil {
				return nil
			}
			return s.raceEngine.GetAllStatusJSON()
		}},
	} {
		remote := hasRole(m.role)
		stream(m.mode, m.local(), !remote)
		if remote {
			stream(m.mode, agentRows(m.role), true)
		}
	}
}
