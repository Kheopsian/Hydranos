package api

import (
	"bytes"
	"encoding/json"
	"fmt"
	"sync"
	"time"
)

// Incremental SSE reconnect. A client returning to a still-open tab already
// holds the hoard list; instead of re-streaming ~100k rows it sends
// ?since=<server_ts it last saw> and we reply with just the delta:
//   - added   torrents (full current row, looked up by info_hash)
//   - removed torrents (info_hash only)
// observed since that cursor. If we can't guarantee completeness for that
// cursor (too old / evicted / server restarted) we fall back to a full
// hydration.
//
// Both adds and removes are timestamped by SERVER observation time (when the
// event crossed the hub), never the torrent's self-reported added_time — that
// field is 0 for a freshly-added torrent, which is exactly the case a delta
// must catch.

type rcEvent struct {
	hash  string
	at    float64 // unix seconds, server-observed
	added bool    // true = torrent_added, false = torrent_removed
}

type reconnectState struct {
	mu    sync.Mutex
	ring  []rcEvent
	cap   int
	floor float64 // earliest unix time from which the delta is still complete
}

func newReconnectState() *reconnectState {
	return &reconnectState{cap: 16384, floor: float64(time.Now().Unix())}
}

func (r *reconnectState) record(hash string, added bool) {
	if hash == "" {
		return
	}
	now := float64(time.Now().Unix())
	r.mu.Lock()
	r.ring = append(r.ring, rcEvent{hash: hash, at: now, added: added})
	if len(r.ring) > r.cap {
		drop := len(r.ring) - r.cap
		r.floor = r.ring[drop-1].at // everything up to here is no longer guaranteed
		r.ring = append([]rcEvent(nil), r.ring[drop:]...)
	}
	r.mu.Unlock()
}

// changesSince returns adds/removes observed after `since`, and whether the
// delta is trustworthy (since within the guaranteed window).
func (r *reconnectState) changesSince(since float64) (added, removed []string, ok bool) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if since < r.floor {
		return nil, nil, false
	}
	for _, e := range r.ring {
		if e.at <= since {
			continue
		}
		if e.added {
			added = append(added, e.hash)
		} else {
			removed = append(removed, e.hash)
		}
	}
	return added, removed, true
}

// startReconnectWatcher records torrent_added / torrent_removed off the hoard
// hub. A cheap byte prefilter skips the 1Hz stats frames without a JSON parse.
func (s *Server) startReconnectWatcher() {
	if s.hoardEngine == nil {
		return
	}
	hub := s.hoardEngine.EventHub()
	if hub == nil {
		return
	}
	mAdd := []byte(`"torrent_added"`)
	mRem := []byte(`"torrent_removed"`)
	go func() {
		_, ch := hub.Subscribe()
		for frame := range ch {
			isAdd := bytes.Contains(frame, mAdd)
			isRem := bytes.Contains(frame, mRem)
			if !isAdd && !isRem {
				continue
			}
			var env struct {
				Event string `json:"event"`
				Data  struct {
					InfoHash string `json:"info_hash"`
				} `json:"data"`
			}
			if json.Unmarshal(frame, &env) != nil {
				continue
			}
			switch env.Event {
			case "torrent_added":
				s.reconnect.record(env.Data.InfoHash, true)
			case "torrent_removed":
				s.reconnect.record(env.Data.InfoHash, false)
			}
		}
	}()
}

// streamDelta emits the incremental update for a reconnecting client. Returns
// false when a full hydration is required instead.
func (s *Server) streamDelta(w interface{ Write([]byte) (int, error) }, flusher interface{ Flush() }, since float64) bool {
	if s.reconnect == nil {
		return false
	}
	// The delta only tracks this node's own engines. With agents registered, a
	// torrent that arrived on one since the last connect would be missing from
	// the delta and stay invisible until a full reload, so decline and let the
	// caller hydrate in full -- which does aggregate agents.
	if len(s.agentsSnapshot()) > 0 {
		return false
	}
	added, removed, ok := s.reconnect.changesSince(since)
	if !ok {
		return false
	}
	// Materialize full rows for the added hashes still present in the list.
	var addRows []map[string]interface{}
	if len(added) > 0 && s.hoardEngine != nil {
		want := make(map[string]bool, len(added))
		for _, h := range added {
			want[h] = true
		}
		for _, t := range s.hoardEngine.GetTorrentList() {
			if h, _ := t["info_hash"].(string); want[h] {
				addRows = append(addRows, t)
			}
		}
	}
	rcWriteFrame(w, map[string]interface{}{"event": "sync", "data": map[string]interface{}{"mode": "delta"}})
	if len(addRows) > 0 {
		rcWriteFrame(w, map[string]interface{}{
			"event": "torrent_batch",
			"data":  map[string]interface{}{"mode": "hoard", "torrents": addRows, "done": true},
		})
	}
	for _, h := range removed {
		rcWriteFrame(w, map[string]interface{}{"event": "torrent_removed", "data": map[string]interface{}{"info_hash": h}})
	}
	flusher.Flush()
	return true
}

func rcWriteFrame(w interface{ Write([]byte) (int, error) }, obj map[string]interface{}) {
	if frame, err := json.Marshal(obj); err == nil {
		fmt.Fprintf(w, "data: %s\n\n", frame)
	}
}
