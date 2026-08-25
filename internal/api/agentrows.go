package api

import (
	"encoding/json"
	"sync"
	"time"
)

// Remote-agent hoard rows for the web UI.
//
// The hoard list is hydrated and kept live entirely over SSE (see sse.go), and
// that stream carries the local engine's torrents only. A torrent placed on an
// agent therefore ran, announced and answered over /api/hoard/torrents while
// being absent from /#hoard — no row to see, select or act on. The race tab
// never had the problem: it still fetches /api/race/torrents, which aggregates
// agents on every call.
//
// Rather than have the front subscribe to every agent's event stream, the rows
// are polled here on one loop and published into the same hub the SSE handler
// already serves: partial torrent_batch frames (upsert-only — see the app.js
// handler) plus torrent_removed for what disappeared. One cache then feeds the
// SSE push, the hydration on connect, /api/hoard/torrents and the tab header
// alike, so what the agents are asked does not scale with the number of open
// browsers.
const (
	agentRowInterval = 5 * time.Second
	// Above this many rows the poll drops to the cadence the hoard table used
	// before it moved to SSE: re-listing a six-figure agent every few seconds
	// to keep its rates fresh is not a trade worth making.
	agentRowLiveMax  = 5000
	agentRowBackstop = 30 * time.Second
)

// agentRowCache holds the last poll, keyed by agent name so an agent that goes
// quiet keeps its rows instead of having them deleted out from under the user.
type agentRowCache struct {
	mu      sync.RWMutex
	byAgent map[string][]map[string]interface{}
	at      time.Time
	pollMu  sync.Mutex // serialises the polls themselves
}

// refreshAgentHoardRows polls every agent's hoard engines and replaces the
// cache. Serialised: two callers polling at once would each diff against a
// different snapshot, and the pusher's view of what disappeared would depend
// on who won.
func (s *Server) refreshAgentHoardRows() []map[string]interface{} {
	return s.pollAgentHoardRows(false)
}

// staleAgentRows reports whether the cache has not been polled for longer than
// the slowest cadence the pusher ever uses.
func (s *Server) staleAgentRows() bool {
	s.agentRows.mu.RLock()
	defer s.agentRows.mu.RUnlock()
	return time.Since(s.agentRows.at) > agentRowBackstop
}

// forceRefreshAgentHoardRows polls unconditionally. The pusher is what keeps
// the cache fresh, so it must never coalesce onto a poll it did not make.
func (s *Server) forceRefreshAgentHoardRows() []map[string]interface{} {
	return s.pollAgentHoardRows(true)
}

func (s *Server) pollAgentHoardRows(force bool) []map[string]interface{} {
	s.agentRows.pollMu.Lock()
	defer s.agentRows.pollMu.Unlock()

	// Someone else may have polled while we waited for the lock. Without this
	// re-check, N browsers connecting at once cost N full polls back to back,
	// each up to one 4s timeout per unreachable agent.
	s.agentRows.mu.RLock()
	fresh := time.Since(s.agentRows.at) < agentRowInterval
	s.agentRows.mu.RUnlock()
	if fresh && !force {
		return s.allAgentRows()
	}

	next := map[string][]map[string]interface{}{}
	for _, ra := range s.agentsSnapshot() {
		// This node's own engines are NOT collected here, even though they are
		// registered agents now. Everything below is added ON TOP of the local
		// counters (see the add() in agentStatusInto), so collecting them would
		// count every local torrent twice: once directly, once as an agent.
		//
		// That is not hypothetical. 3.135.0 shipped without this line and
		// /api/status reported exactly double -- 396592 torrents for the 198296
		// the database holds -- because making "local" a registered agent
		// silently enrolled it here. Rolled back within minutes; nothing was
		// written, info_hash is a primary key so the rows could not duplicate,
		// but every count and every listing was wrong.
		//
		// The lasting fix is for the local path to stop contributing and let
		// everything come from agent rows, which is where this is heading. Until
		// then, exactly one of the two must feed the totals.
		if ra.local {
			continue
		}
		engines := ra.byRole("hoard")
		if len(engines) == 0 {
			continue
		}
		var got []map[string]interface{}
		ok := true
		for _, e := range engines {
			lst, err := e.client.ListTorrentsTimeout(4 * time.Second)
			if err != nil || lst == nil {
				ok = false
				continue
			}
			cats, _ := e.client.TorrentCategories(e.id)
			for _, t := range lst.Torrents {
				row := ltStatusToRow(t, ra.name, cats)
				// Which engine on that agent holds it: an agent may host
				// several hoard engines, and a routed action has to name the
				// right one rather than defaulting to the id "hoard".
				row["agent_engine"] = e.id
				got = append(got, row)
			}
		}
		if !ok {
			// Unreachable or slow this tick: keep what we already had for it.
			// A blip must not empty the table and fire a remove per torrent.
			if prev := s.agentRowsFor(ra.name); prev != nil {
				next[ra.name] = prev
			}
			continue
		}
		next[ra.name] = got
	}

	s.agentRows.mu.Lock()
	s.agentRows.byAgent = next
	s.agentRows.at = time.Now()
	s.agentRows.mu.Unlock()

	var rows []map[string]interface{}
	for _, list := range next {
		rows = append(rows, list...)
	}
	return rows
}

// allAgentRows flattens the cache. Callers hold no lock.
func (s *Server) allAgentRows() []map[string]interface{} {
	s.agentRows.mu.RLock()
	defer s.agentRows.mu.RUnlock()
	var rows []map[string]interface{}
	for _, list := range s.agentRows.byAgent {
		rows = append(rows, list...)
	}
	return rows
}

// agentRowsFor returns the cached rows of one agent, or nil.
func (s *Server) agentRowsFor(name string) []map[string]interface{} {
	s.agentRows.mu.RLock()
	defer s.agentRows.mu.RUnlock()
	return s.agentRows.byAgent[name]
}

// agentHoardRows returns every agent's hoard rows, refreshing when the cache is
// older than one push interval. A request path therefore costs at most one
// round trip per agent, and never serves a list frozen from whenever the last
// browser happened to be connected.
func (s *Server) agentHoardRows() []map[string]interface{} {
	s.agentRows.mu.RLock()
	fresh := time.Since(s.agentRows.at) < agentRowInterval
	s.agentRows.mu.RUnlock()
	if !fresh {
		return s.refreshAgentHoardRows()
	}
	return s.allAgentRows()
}

// startAgentRowPusher keeps the rows of an open /#hoard live, since no agent
// event reaches this node's hub on its own. Silent while no browser is on
// /api/events: nobody would read the push, and the request paths refresh the
// cache themselves.
func (s *Server) startAgentRowPusher() {
	if s.hoardEngine == nil {
		return
	}
	hub := s.hoardEngine.EventHub()
	if hub == nil {
		return
	}
	go func() {
		delay := agentRowInterval
		// What the last push put on screen. Removals are diffed against this
		// rather than against the cache, so a refresh from a request path in
		// between cannot swallow the news that a torrent is gone.
		pushed := map[string]bool{}
		for {
			time.Sleep(delay)
			delay = agentRowInterval
			if s.sseClients.Load() == 0 {
				continue
			}
			rows := s.forceRefreshAgentHoardRows()
			live := make(map[string]bool, len(rows))
			for _, r := range rows {
				if h, _ := r["info_hash"].(string); h != "" {
					live[h] = true
				}
			}
			var gone []string
			for h := range pushed {
				if !live[h] {
					gone = append(gone, h)
				}
			}
			pushed = live
			// A listing is the whole list, so past a certain size re-asking
			// every few seconds costs more than the live rates are worth. Back
			// off to the cadence the pre-SSE table polled at, and let the row
			// count decide rather than a setting nobody would know to turn.
			if len(rows) > agentRowLiveMax {
				delay = agentRowBackstop
			}
			if len(rows) == 0 && len(gone) == 0 {
				continue
			}
			const chunk = 1000
			for i := 0; i < len(rows); i += chunk {
				end := i + chunk
				if end > len(rows) {
					end = len(rows)
				}
				// partial: these frames carry a subset of the list, so they
				// must never end a hydration or prune what they don't carry.
				if frame, err := json.Marshal(map[string]interface{}{
					"event": "torrent_batch",
					"data": map[string]interface{}{
						"mode": "hoard", "torrents": rows[i:end], "partial": true,
					},
				}); err == nil {
					hub.Publish(frame)
				}
			}
			for _, h := range gone {
				if frame, err := json.Marshal(map[string]interface{}{
					"event": "torrent_removed",
					"data":  map[string]interface{}{"info_hash": h},
				}); err == nil {
					hub.Publish(frame)
				}
			}
		}
	}()
}

// agentHoardOwner resolves which agent (and which of its engines) holds a hoard
// torrent, from the cached rows — no round trip, which is what makes acting on
// a whole selection affordable. Only knows what the last poll listed.
func (s *Server) agentHoardOwner(hash string) (agent, engineID string, ok bool) {
	s.agentRows.mu.RLock()
	defer s.agentRows.mu.RUnlock()
	for name, list := range s.agentRows.byAgent {
		for _, r := range list {
			if h, _ := r["info_hash"].(string); h == hash {
				id, _ := r["agent_engine"].(string)
				return name, id, true
			}
		}
	}
	return "", "", false
}

// numOf reads a row/status number whatever concrete type it arrived as.
func numOf(v interface{}) int64 {
	switch n := v.(type) {
	case int:
		return int64(n)
	case int64:
		return n
	case float64:
		return int64(n)
	}
	return 0
}

// mergeAgentHoardStats folds the agents' torrents into the /#hoard header, so
// the summary above the table counts the same torrents the table lists. Reads
// the cached rows only — it is called at 1Hz by the snapshot pusher.
//
// Deliberately limited to counts, peers and rates. Session and baseline totals
// stay node-local: they feed announce accounting, which belongs to the node
// that actually announces.
func (s *Server) mergeAgentHoardStats(status map[string]interface{}) {
	// With no browser on /api/events the pusher stops polling, so the cache
	// freezes -- and these counts would keep reporting rates and peers from
	// whenever the last one disconnected. A REST caller asking for the status
	// pays a refresh instead, at most one per backstop interval.
	if s.staleAgentRows() {
		s.refreshAgentHoardRows()
	}

	s.agentRows.mu.RLock()
	defer s.agentRows.mu.RUnlock()

	var count, withPeers, uploading, announced, peers, swarm, ulRate, dlRate int64
	for _, list := range s.agentRows.byAgent {
		for _, r := range list {
			count++
			np, ur, dr := numOf(r["num_peers"]), numOf(r["upload_rate"]), numOf(r["download_rate"])
			peers += np
			swarm += numOf(r["swarm_leechers"])
			ulRate += ur
			dlRate += dr
			if np > 0 {
				withPeers++
			}
			if ur > 0 {
				uploading++
			}
			// Inferred, not reported: the agent owns the real announce state
			// and its listing does not carry it. A hoard torrent with no
			// tracker error is one its announcer is keeping alive.
			if e, _ := r["tracker_error"].(bool); !e {
				announced++
			}
		}
	}
	if count == 0 {
		return
	}
	add := func(key string, n int64) {
		status[key] = numOf(status[key]) + n
	}
	add("total_torrents", count)
	add("torrents_with_peers", withPeers)
	add("torrents_uploading", uploading)
	add("torrents_announced", announced)
	add("active_peers", peers)
	add("swarm_leechers", swarm)
	add("active_upload_rate", ulRate)
	add("active_download_rate", dlRate)
}
