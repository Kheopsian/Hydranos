package api

import (
	"encoding/json"
	"log/slog"
	"time"
)

// startSnapshotPusher publishes status_snapshot and hoard_stats_snapshot
// every second on the existing hoard EventHub. The frontend consumes them
// over /api/events to avoid the 1Hz HTTP poll storm that was queuing up
// requests under bad wifi.
//
// No-op if the hoard engine or hub isn't ready (called once during boot).
func (s *Server) startSnapshotPusher() {
	if s.hoardEngine == nil {
		return
	}
	hub := s.hoardEngine.EventHub()
	if hub == nil {
		slog.Info("snapshot pusher: no event hub, skipping")
		return
	}

	go func() {
		t := time.NewTicker(1 * time.Second)
		defer t.Stop()
		for range t.C {
			if hub.NumSubs() == 0 {
				continue
			}

			if frame, err := json.Marshal(map[string]interface{}{
				"event": "status_snapshot",
				"data":  s.statusPayload(),
			}); err == nil {
				hub.Publish(frame)
			}

			if frame, err := json.Marshal(map[string]interface{}{
				"event": "hoard_stats_snapshot",
				"data":  s.hoardStatsPayload(),
			}); err == nil {
				hub.Publish(frame)
			}
		}
	}()
	slog.Info("snapshot pusher: started (1Hz status + hoard_stats over SSE)")
}
