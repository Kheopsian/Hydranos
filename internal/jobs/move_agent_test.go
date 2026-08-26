package jobs

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/store"
)

type fakeMover struct {
	started  int
	statuses []agentwire.MoveStatus
	startErr error
	polls    int
}

func (f *fakeMover) MovePayload(agentwire.MovePayloadParams) (agentwire.MoveStatus, error) {
	f.started++
	if f.startErr != nil {
		return agentwire.MoveStatus{}, f.startErr
	}
	if len(f.statuses) > 0 {
		return f.statuses[0], nil
	}
	return agentwire.MoveStatus{Running: true, Total: 100}, nil
}

// Each poll advances one step through the script, so a test reads as the
// sequence the node would actually report.
func (f *fakeMover) MoveStatus(string) (agentwire.MoveStatus, error) {
	if len(f.statuses) == 0 {
		return agentwire.MoveStatus{}, nil
	}
	i := f.polls
	f.polls++
	if i >= len(f.statuses) {
		i = len(f.statuses) - 1
	}
	return f.statuses[i], nil
}

func agentMoveJob(t *testing.T) *store.Job {
	t.Helper()
	blob, _ := json.Marshal(AgentMoveParams{Agent: "seedbox", Engine: "hoard-0", TargetDir: "/data/tv", Category: "tv"})
	return &store.Job{ID: "j1", Type: store.JobTypeMoveDataAgent, InfoHash: testIH, Params: string(blob)}
}

func runAgentMove(t *testing.T, m *fakeMover) (int64, int64, error) {
	t.Helper()
	r := &AgentMoveRunner{Dial: func(string, string) (AgentMover, error) { return m, nil }}
	var done, total int64
	err := r.Run(context.Background(), agentMoveJob(t), func(d, tt int64) { done, total = d, tt })
	return done, total, err
}

// The node owns the files; this side owns the record. A move that finishes
// there has to finish here, with the progress the node reported.
func TestAgentMoveFollowsTheNodeToCompletion(t *testing.T) {
	m := &fakeMover{statuses: []agentwire.MoveStatus{
		{Running: true, Total: 100},
		{Running: true, Done: 50, Total: 100},
		{Finished: true, Done: 100, Total: 100, Target: "/data/tv/x"},
	}}
	done, total, err := runAgentMove(t, m)
	if err != nil {
		t.Fatalf("run: %v", err)
	}
	if done != 100 || total != 100 {
		t.Errorf("progress = %d/%d, want 100/100", done, total)
	}
	if m.started != 1 {
		t.Errorf("started the move %d times", m.started)
	}
}

// A failure on the node is this job's failure. Reporting success for a move
// that did not happen would leave the payload in the old category's directory
// with the torrent claiming the new one.
func TestAgentMoveFailsWhenTheNodeDoes(t *testing.T) {
	m := &fakeMover{statuses: []agentwire.MoveStatus{
		{Running: true, Total: 100},
		{Finished: true, Error: "no space left on device"},
	}}
	_, _, err := runAgentMove(t, m)
	if err == nil || !strings.Contains(err.Error(), "no space") {
		t.Fatalf("err = %v, want the node's reason", err)
	}
}

// A move already running there is the resume case: follow it rather than
// starting a second copy of the same payload.
func TestAgentMoveFollowsAMoveAlreadyRunning(t *testing.T) {
	m := &fakeMover{
		startErr: fmt.Errorf("a move of %s is already running here", testIH),
		statuses: []agentwire.MoveStatus{
			{Running: true, Done: 10, Total: 100},
			{Finished: true, Done: 100, Total: 100},
		},
	}
	if _, _, err := runAgentMove(t, m); err != nil {
		t.Fatalf("run: %v", err)
	}
}

// A node that restarted mid-move has no record of it. Reporting that as
// finished is the one answer that loses data silently: the payload is half in
// each directory and nothing says so.
func TestAgentMoveRefusesToCallAForgottenMoveDone(t *testing.T) {
	m := &fakeMover{
		startErr: fmt.Errorf("engine gone"),
		statuses: nil,
	}
	_, _, err := runAgentMove(t, m)
	if err == nil {
		t.Fatal("a move the node knows nothing about was reported as done")
	}
}
