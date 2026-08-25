package api

import (
	"log/slog"
	"sync/atomic"
	"time"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// eventStreamer is the part of an agent client that can push events instead of
// being polled. Deliberately a SEPARATE, optional interface rather than part of
// AgentClient: an engine running in this process does not implement it, so
// local agents are skipped here by construction instead of by an `if ra.local`
// that someone could delete. Their events already reach the front through the
// engine's own hub, and a second server-shaped stream would deliver everything
// twice -- the same duplication that doubled the counters earlier today.
type eventStreamer interface {
	SetEventHandler(func(ltclient.Event))
	SubscribeEvents() error
}

// deltasApplied counts the row updates that came from a stream rather than from
// a re-listing. It exists to answer one question on a real deployment before
// the polling cadence is relaxed: is the stream actually carrying the traffic,
// or is it silently dead and the poll quietly covering for it? Trusting the
// stream without being able to see it work is how a cache goes stale in a way
// nobody notices.
var deltasApplied atomic.Int64

// DeltasApplied reports how many rows the event streams have updated.
func DeltasApplied() int64 { return deltasApplied.Load() }

// subscribeAgentRows opens the event stream of every engine of one agent and
// feeds the row cache from it.
//
// The poll is deliberately left exactly as it was. This change only makes the
// cache fresher BETWEEN polls; relaxing the cadence is a separate step, taken
// once DeltasApplied shows the stream is really doing the work on a live
// system. Doing both at once would mean trusting an untested path with the
// safety net removed in the same commit.
func (s *Server) subscribeAgentRows(ra *remoteAgent) {
	for _, e := range ra.engines {
		streamer, ok := e.client.(eventStreamer)
		if !ok {
			continue // in-process engine: its events come from the hub
		}
		agentName, engineID := ra.name, e.id
		streamer.SetEventHandler(func(ev ltclient.Event) {
			if s.applyAgentEvent(agentName, engineID, ev) {
				// Something a delta cannot express. Ask the next request or
				// push to re-poll rather than inventing a row here.
				s.agentRows.mu.Lock()
				s.agentRows.at = time.Time{}
				s.agentRows.mu.Unlock()
				return
			}
			deltasApplied.Add(1)
		})
		if err := streamer.SubscribeEvents(); err != nil {
			// Not fatal: the poll still fills this agent's rows. Logged at
			// warn because a silently missing stream is exactly the state that
			// looks fine and costs a full re-listing every interval.
			slog.Warn("agent rows: event stream unavailable, falling back to polling",
				"agent", agentName, "engine", engineID, "error", err)
			continue
		}
		slog.Info("agent rows: following the event stream", "agent", agentName, "engine", engineID)
	}
}
