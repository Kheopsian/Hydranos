package jobs

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"time"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/store"
)

// Relocating a payload that lives on another node.
//
// A category change moves a torrent's files. For this node's own primaries it
// always has; for a torrent on an agent it did nothing at all, and the code
// said so outright -- every path involved belongs to that node, so planning
// here compared its paths with ours and refused a cross-filesystem move nobody
// had asked for.
//
// The work happens where the files are. This job is the front's half: it asks
// the node to start, watches how far it gets, and owns the durable record so a
// restart on either side does not lose the operation.

// AgentMover is the node holding the payload, as this job needs it.
type AgentMover interface {
	MovePayload(p agentwire.MovePayloadParams) (agentwire.MoveStatus, error)
	MoveStatus(infoHash string) (agentwire.MoveStatus, error)
}

// AgentMoveParams is the durable description of the request.
type AgentMoveParams struct {
	Agent  string `json:"agent"`
	Engine string `json:"engine,omitempty"`
	// TargetDir is the category's save path ON THAT AGENT, resolved when the
	// job was created. Not re-derived at run time: the category's mapping may
	// have changed since, and a job must do what it was accepted for.
	TargetDir              string `json:"target_dir"`
	Category               string `json:"category,omitempty"`
	Name                   string `json:"name,omitempty"`
	AllowBreakingHardlinks bool   `json:"allow_breaking_hardlinks,omitempty"`
	BytesPerSecond         int64  `json:"bytes_per_second,omitempty"`
}

// AgentMoveRunner drives a move on the node that holds the payload.
type AgentMoveRunner struct {
	// Dial resolves the agent by name at RUN time, never at submission: a
	// resumed job must find the node as it is now.
	Dial func(agent, engine string) (AgentMover, error)
}

func (r *AgentMoveRunner) Type() string { return store.JobTypeMoveDataAgent }

// Resume: always. Starting a move that is already running is refused by the
// node itself, and one that finished while this process was down is reported
// as finished -- so the worst case is a poll that ends immediately.
func (r *AgentMoveRunner) Resume(j *store.Job) (bool, string) {
	var p AgentMoveParams
	if err := json.Unmarshal([]byte(j.Params), &p); err != nil {
		return false, "job params unreadable: " + err.Error()
	}
	return true, ""
}

// agentMovePoll is how often the node is asked how far it got. A move is
// minutes to hours of I/O; a tighter loop would cost a round trip per second
// to learn nothing.
const agentMovePoll = 2 * time.Second

func (r *AgentMoveRunner) Run(ctx context.Context, j *store.Job, report func(done, total int64)) error {
	var p AgentMoveParams
	if err := json.Unmarshal([]byte(j.Params), &p); err != nil {
		return fmt.Errorf("agent move: unreadable params: %w", err)
	}
	cl, err := r.Dial(p.Agent, p.Engine)
	if err != nil {
		return fmt.Errorf("agent move: %s: %w", p.Agent, err)
	}

	st, err := cl.MovePayload(agentwire.MovePayloadParams{
		Engine:                 p.Engine,
		InfoHash:               j.InfoHash,
		TargetDir:              p.TargetDir,
		Category:               p.Category,
		AllowBreakingHardlinks: p.AllowBreakingHardlinks,
		BytesPerSecond:         p.BytesPerSecond,
	})
	if err != nil {
		// A move already running there is the resume case, not a failure: fall
		// through to the poll and follow the one that exists.
		if st2, sErr := cl.MoveStatus(j.InfoHash); sErr == nil && st2.Running {
			slog.Info("agent move: already running on the node, following it",
				"info_hash", j.InfoHash, "agent", p.Agent)
			st = st2
		} else {
			return fmt.Errorf("agent move: %w", err)
		}
	}
	if st.Finished && st.Error == "" {
		report(st.Total, st.Total)
		return nil
	}
	report(st.Done, st.Total)

	tick := time.NewTicker(agentMovePoll)
	defer tick.Stop()
	for {
		select {
		case <-ctx.Done():
			// The node keeps moving: it owns the operation, and stopping it
			// from here would leave a half-copied payload with nobody watching.
			// A resumed job picks the same move back up.
			return ctx.Err()
		case <-tick.C:
			st, err := cl.MoveStatus(j.InfoHash)
			if err != nil {
				return fmt.Errorf("agent move: %s stopped answering: %w", p.Agent, err)
			}
			report(st.Done, st.Total)
			if st.Error != "" {
				return fmt.Errorf("agent move: %s: %s", p.Agent, st.Error)
			}
			if st.Finished {
				slog.Info("agent move: payload relocated on the node",
					"info_hash", j.InfoHash, "agent", p.Agent, "target", st.Target)
				return nil
			}
			if !st.Running && st.Total == 0 && st.Done == 0 {
				// The node has no record of it: it restarted mid-move. Nothing
				// was lost -- the mover stages into the target and the source
				// is only removed at the end -- so ask again.
				return fmt.Errorf("agent move: %s no longer knows about this move (it restarted); retry it", p.Agent)
			}
		}
	}
}
