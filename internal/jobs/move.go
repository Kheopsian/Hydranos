package jobs

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"

	"github.com/Kheopsian/hydra/internal/move"
	"github.com/Kheopsian/hydra/internal/store"
)

// PayloadHost is what a move needs from the engine holding the torrent.
//
// Deliberately narrow, and named for what it does rather than for which engine
// it is: a move does not care whether it is talking to race or hoard, only
// that something can stop the torrent, be told where its data now lives, and
// start it again.
type PayloadHost interface {
	HasTorrent(infoHash string) bool
	StopTorrent(infoHash string) error
	StartTorrent(infoHash string) error
	// SetEngineSavePath points the engine at the payload's new location and
	// updates Hydra's own bookkeeping. The files must already be there.
	SetEngineSavePath(infoHash, engineSavePath, onDiskRoot, category string) error
}

// MoveParams is the durable description of a move. It is stored as the job's
// params, so a resumed job knows exactly what it was asked to do -- including
// whether the operator had accepted breaking hardlinks, which must not be
// silently re-answered on resume.
type MoveParams struct {
	// Source and Target are the torrent's on-disk content roots.
	Source string `json:"source"`
	Target string `json:"target"`
	// EngineSavePath is what the engine is told afterwards: the parent of
	// the content root for a multi-file torrent, the root itself otherwise.
	EngineSavePath string `json:"engine_save_path"`
	Category       string `json:"category"`
	// Name is the torrent's name, copied in when the job is created. The job
	// outlives the torrent, and a hash alone is not something anyone can read
	// or search for.
	Name string `json:"name,omitempty"`
	// AllowBreakingHardlinks is the operator's explicit answer, captured at
	// submission time.
	AllowBreakingHardlinks bool `json:"allow_breaking_hardlinks"`
	// BytesPerSecond caps the copy. Zero is uncapped.
	BytesPerSecond int64 `json:"bytes_per_second,omitempty"`
	// Loose marks a payload with no folder of its own, whose Source is a
	// category directory shared with other torrents. Files then lists what
	// belongs to this torrent; nothing else in Source may be touched.
	Loose bool `json:"loose,omitempty"`
	// Files are the payload's paths relative to Source. Stored rather than
	// re-derived so a resumed job moves what it was asked to move, even if
	// the engine has since forgotten the torrent.
	Files []string `json:"files,omitempty"`
}

// MoveRunner relocates torrent payloads.
type MoveRunner struct {
	// Host resolves the engine currently holding a torrent. Returning nil
	// means nothing holds it, which aborts the move rather than moving data
	// out from under an engine that has forgotten about it.
	Host func(infoHash string) PayloadHost
}

func (r *MoveRunner) Type() string { return store.JobTypeMoveData }

// Resume decides whether an interrupted move can be picked up.
//
// It always can, and safely, because of how Execute is ordered: everything
// before the swap is a copy into a staging directory that a re-run reuses, and
// the source is only ever removed after the target is verified and in place.
// The worst case is re-verifying files that were already copied.
func (r *MoveRunner) Resume(j *store.Job) (bool, string) {
	var p MoveParams
	if err := json.Unmarshal([]byte(j.Params), &p); err != nil {
		return false, "job params unreadable: " + err.Error()
	}
	if p.Loose {
		// The source is a shared directory: it exists whether or not this
		// move ran, so its presence says nothing. Where the payload's own
		// files are is what tells the two apart.
		atSource, atTarget := 0, 0
		for _, rel := range p.Files {
			if _, err := os.Stat(filepath.Join(p.Source, rel)); err == nil {
				atSource++
			}
			if _, err := os.Stat(filepath.Join(p.Target, rel)); err == nil {
				atTarget++
			}
		}
		if atSource == 0 && len(p.Files) > 0 && atTarget == len(p.Files) {
			return false, "already completed before the restart: payload is at " + p.Target
		}
		if atSource == 0 {
			return false, "payload is no longer at " + p.Source
		}
		return true, ""
	}
	if _, err := os.Stat(p.Source); err != nil {
		// The source is gone. Either the move finished and the process died
		// between the removal and the state write -- in which case there is
		// nothing left to do -- or something else removed it, and re-running
		// would be guesswork either way.
		if _, terr := os.Stat(p.Target); terr == nil {
			return false, "already completed before the restart: payload is at " + p.Target
		}
		return false, "source no longer exists: " + p.Source
	}
	return true, ""
}

func (r *MoveRunner) Run(ctx context.Context, j *store.Job, report func(done, total int64)) error {
	var p MoveParams
	if err := json.Unmarshal([]byte(j.Params), &p); err != nil {
		return fmt.Errorf("move job: unreadable params: %w", err)
	}

	host := r.Host(j.InfoHash)
	if host == nil {
		return fmt.Errorf("move job: no engine holds %s", j.InfoHash)
	}

	var plan *move.Plan
	var err error
	if p.Loose {
		plan, err = move.InspectLoose(p.Source, p.Target, j.InfoHash, p.Files)
	} else {
		plan, err = move.Inspect(p.Source, p.Target)
	}
	if err != nil {
		return err
	}
	report(0, plan.TotalBytes)

	if plan.SameFS {
		slog.Info("move job: same filesystem, renaming",
			"info_hash", j.InfoHash, "from", p.Source, "to", p.Target)
	} else {
		slog.Info("move job: copying across filesystems, torrent keeps seeding",
			"info_hash", j.InfoHash, "from", p.Source, "to", p.Target,
			"bytes", plan.TotalBytes, "hardlinks_broken", plan.HardlinkedFiles)
	}

	stopped := false
	err = move.Execute(ctx, plan, move.Options{
		AllowBreakingHardlinks: p.AllowBreakingHardlinks,
		BytesPerSecond:         p.BytesPerSecond,
		OnProgress:             func(done int64) { report(done, plan.TotalBytes) },

		// The torrent seeds for the whole copy and stops only here, for the
		// rename and the engine call.
		BeforeSwap: func() error {
			if err := host.StopTorrent(j.InfoHash); err != nil {
				return fmt.Errorf("stopping the torrent: %w", err)
			}
			stopped = true
			return nil
		},
		AfterSwap: func() error {
			if err := host.SetEngineSavePath(j.InfoHash, p.EngineSavePath, p.Target, p.Category); err != nil {
				return err
			}
			if err := host.StartTorrent(j.InfoHash); err != nil {
				return fmt.Errorf("restarting the torrent: %w", err)
			}
			stopped = false
			return nil
		},
		// Called instead of AfterSwap when the swap is called off: the
		// payload never left the source and the engine still points at it,
		// so the torrent only has to go back to work.
		AbortSwap: func() error {
			if err := host.StartTorrent(j.InfoHash); err != nil {
				return fmt.Errorf("restarting the torrent: %w", err)
			}
			stopped = false
			return nil
		},
	})

	if err != nil {
		// A torrent stopped for a swap that then failed must not be left
		// stopped: its data is still at the source, so starting it there is
		// both correct and the least surprising outcome.
		if stopped {
			if serr := host.StartTorrent(j.InfoHash); serr != nil {
				slog.Error("move job: failed and the torrent could not be restarted",
					"info_hash", j.InfoHash, "error", serr)
			}
		}
		if ctx.Err() != nil {
			// Cancelled: drop the half-built copy so the next attempt is not
			// misled by it and the space is returned.
			if cerr := move.CleanupStagingAt(plan.Staging); cerr != nil {
				slog.Warn("move job: could not clean up staging", "target", p.Target, "error", cerr)
			}
		}
		return err
	}

	slog.Info("move job: payload relocated", "info_hash", j.InfoHash, "to", p.Target)
	return nil
}
