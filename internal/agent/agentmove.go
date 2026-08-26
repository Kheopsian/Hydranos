package agent

import (
	"context"
	"fmt"
	"log/slog"
	"path/filepath"
	"sync"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/move"
)

// Relocating a payload on the node that holds it.
//
// A category change moves a torrent's files. On this node's own engines it
// always has; on an agent it did nothing at all, because every path involved
// belongs to the agent and only the agent can see them -- planning from the
// front measured a Windows path against a Linux one and refused a
// cross-filesystem move nobody had asked for.
//
// So the front keeps the decision and the job, and the node does the work: it
// plans in its own filesystem, moves in the background because a copy across
// filesystems takes hours, and reports how far it got.

// payloadMover is what a move needs from the engine holding the torrent. The
// agent's own RaceEngine/HoardEngine satisfy it, which is why nothing here
// duplicates the mover the front runs for its primaries -- it is the same
// internal/move package, driven from the other side of the wire.
type payloadMover interface {
	HasTorrent(infoHash string) bool
	StopTorrent(infoHash string) error
	StartTorrent(infoHash string) error
	SetEngineSavePath(infoHash, engineSavePath, onDiskRoot, category string) error
	GetTorrentDetail(infoHash string) map[string]interface{}
}

// moveState is one running (or finished) relocation, keyed by info hash.
type moveState struct {
	done     int64
	total    int64
	running  bool
	finished bool
	err      string
	target   string
}

type moveTracker struct {
	mu sync.Mutex
	m  map[string]*moveState
}

func (t *moveTracker) get(ih string) *moveState {
	t.mu.Lock()
	defer t.mu.Unlock()
	if st := t.m[ih]; st != nil {
		cp := *st
		return &cp
	}
	return nil
}

func (t *moveTracker) start(ih, target string, total int64) bool {
	t.mu.Lock()
	defer t.mu.Unlock()
	if t.m == nil {
		t.m = map[string]*moveState{}
	}
	if st := t.m[ih]; st != nil && st.running {
		return false // already moving; a second start would copy over the first
	}
	t.m[ih] = &moveState{running: true, total: total, target: target}
	return true
}

func (t *moveTracker) progress(ih string, done, total int64) {
	t.mu.Lock()
	defer t.mu.Unlock()
	if st := t.m[ih]; st != nil {
		st.done, st.total = done, total
	}
}

func (t *moveTracker) finish(ih string, err error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	st := t.m[ih]
	if st == nil {
		return
	}
	st.running, st.finished = false, true
	if err != nil {
		st.err = err.Error()
	}
}

// moverFor resolves the engine holding a torrent here, by id when one is named
// and by search otherwise.
func (s *Server) moverFor(engineID, infoHash string) (payloadMover, error) {
	try := func(id string) payloadMover {
		r := s.resolveRich(id)
		if r == nil {
			return nil
		}
		m, ok := r.(payloadMover)
		if !ok {
			return nil
		}
		if !m.HasTorrent(infoHash) {
			return nil
		}
		return m
	}
	if engineID != "" {
		if m := try(engineID); m != nil {
			return m, nil
		}
	}
	s.enginesMu.RLock()
	ids := make([]string, 0, len(s.rich))
	for id := range s.rich {
		ids = append(ids, id)
	}
	s.enginesMu.RUnlock()
	for _, id := range ids {
		if m := try(id); m != nil {
			return m, nil
		}
	}
	return nil, fmt.Errorf("no engine on this node holds %s", infoHash)
}

// movePaths works out what would move where, in this node's own filesystem.
func movePaths(m payloadMover, infoHash, targetDir string) (src, dst, engineSavePath string, err error) {
	detail := m.GetTorrentDetail(infoHash)
	if detail == nil {
		return "", "", "", fmt.Errorf("torrent %s has no detail to move", infoHash)
	}
	src, _ = detail["save_path"].(string)
	if src == "" {
		return "", "", "", fmt.Errorf("torrent %s has no save_path", infoHash)
	}
	multi, _ := detail["multi_file"].(bool)
	dst = filepath.Join(targetDir, filepath.Base(src))
	if multi {
		engineSavePath = filepath.Dir(dst)
	} else {
		engineSavePath = dst
	}
	return filepath.Clean(src), filepath.Clean(dst), engineSavePath, nil
}

// handleMovePlan answers what a move would involve, without touching anything.
func (s *Server) handleMovePlan(p agentwire.MovePlanParams) (agentwire.MovePlan, error) {
	m, err := s.moverFor(p.Engine, p.InfoHash)
	if err != nil {
		return agentwire.MovePlan{}, err
	}
	src, dst, _, err := movePaths(m, p.InfoHash, p.TargetDir)
	if err != nil {
		return agentwire.MovePlan{}, err
	}
	if src == dst {
		return agentwire.MovePlan{Source: src, Target: dst, NothingToDo: true}, nil
	}
	plan, err := move.Inspect(src, dst)
	if err != nil {
		return agentwire.MovePlan{}, err
	}
	out := agentwire.MovePlan{
		Source:          plan.Source,
		Target:          plan.Target,
		TotalBytes:      plan.TotalBytes,
		FreeBytes:       plan.FreeBytes,
		SameFilesystem:  plan.SameFS,
		HardlinkedFiles: plan.HardlinkedFiles,
		HardlinkedBytes: plan.HardlinkedBytes,
		HardlinkExample: plan.HardlinkExamples,
	}
	// Reported, not returned as an error: the front turns "hardlinks" into a
	// question and asks again with the answer.
	if cErr := plan.Check(false); cErr != nil {
		out.Blocked = cErr.Error()
	}
	return out, nil
}

// handleMovePayload starts a move and returns immediately. A cross-filesystem
// copy runs for hours; holding the RPC open for it would time out on every
// layer between here and the browser.
func (s *Server) handleMovePayload(p agentwire.MovePayloadParams) (agentwire.MoveStatus, error) {
	m, err := s.moverFor(p.Engine, p.InfoHash)
	if err != nil {
		return agentwire.MoveStatus{}, err
	}
	src, dst, engineSavePath, err := movePaths(m, p.InfoHash, p.TargetDir)
	if err != nil {
		return agentwire.MoveStatus{}, err
	}
	if src == dst {
		// Still tell the engine which category it is in: the label is the
		// caller's real intent and the files simply happen to be in place.
		if sErr := m.SetEngineSavePath(p.InfoHash, engineSavePath, dst, p.Category); sErr != nil {
			return agentwire.MoveStatus{}, sErr
		}
		return agentwire.MoveStatus{Finished: true, Target: dst}, nil
	}
	plan, err := move.Inspect(src, dst)
	if err != nil {
		return agentwire.MoveStatus{}, err
	}
	if cErr := plan.Check(p.AllowBreakingHardlinks); cErr != nil {
		return agentwire.MoveStatus{}, cErr
	}
	if !s.moves.start(p.InfoHash, dst, plan.TotalBytes) {
		return agentwire.MoveStatus{}, fmt.Errorf("a move of %s is already running here", p.InfoHash)
	}

	go func() {
		err := s.runMove(m, p, plan, engineSavePath)
		s.moves.finish(p.InfoHash, err)
		if err != nil {
			slog.Error("agent move: failed", "info_hash", p.InfoHash, "target", plan.Target, "error", err)
			return
		}
		slog.Info("agent move: payload relocated", "info_hash", p.InfoHash, "target", plan.Target)
	}()
	return agentwire.MoveStatus{Running: true, Total: plan.TotalBytes, Target: dst}, nil
}

// runMove is the sequence the front runs for its own engines, on this side of
// the wire: stop, move the bytes, tell the engine where they are, start again.
// The torrent is stopped first because an engine writing into files that are
// being moved is how a payload ends up half in each place.
func (s *Server) runMove(m payloadMover, p agentwire.MovePayloadParams, plan *move.Plan, engineSavePath string) error {
	if err := m.StopTorrent(p.InfoHash); err != nil {
		return fmt.Errorf("stopping the torrent: %w", err)
	}
	// Restarted whatever happens next: a failed move must not leave the
	// torrent stopped and silent, seeding nothing, until someone notices.
	defer func() {
		if err := m.StartTorrent(p.InfoHash); err != nil {
			slog.Warn("agent move: the torrent could not be restarted", "info_hash", p.InfoHash, "error", err)
		}
	}()

	err := move.Execute(context.Background(), plan, move.Options{
		AllowBreakingHardlinks: p.AllowBreakingHardlinks,
		BytesPerSecond:         p.BytesPerSecond,
		OnProgress: func(done int64) {
			s.moves.progress(p.InfoHash, done, plan.TotalBytes)
		},
	})
	if err != nil {
		return err
	}
	return m.SetEngineSavePath(p.InfoHash, engineSavePath, plan.Target, p.Category)
}

// handleMoveStatus reports how far a move got. An unknown hash is not an error:
// the front polls, and a job resumed after this process restarted has every
// right to ask about a move that no longer exists here.
func (s *Server) handleMoveStatus(p agentwire.MoveStatusParams) agentwire.MoveStatus {
	st := s.moves.get(p.InfoHash)
	if st == nil {
		return agentwire.MoveStatus{}
	}
	return agentwire.MoveStatus{
		Running: st.running, Done: st.done, Total: st.total,
		Finished: st.finished, Error: st.err, Target: st.target,
	}
}
