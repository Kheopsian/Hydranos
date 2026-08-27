package main

import (
	"context"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"

	"github.com/Kheopsian/hydra/internal/api"
	"github.com/Kheopsian/hydra/internal/choking"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/store"
)

// startExtraEngines spawns every engine the monolith hosts BEYOND its primary
// race+hoard pair (Option A sharding: e.g. a second/third hoard to spread the
// per-torrent announce goroutine cost across processes). Each extra engine runs
// the same lifecycle as a dedicated agent engine -- its own Typhon process,
// per-engine store, tracker announcer and periodic reconcile -- and seeds
// independently.
//
// The primary race/hoard (already wired into api.Server) are identified by the
// SessionConfig pointers raceCfg/hoardCfg (which point into engineCfgs) and
// skipped. A legacy default.toml has no extras, so this only builds the manager.
//
// Returns the manager that owns them: the set is no longer decided at boot,
// since adding an engine from the Agents page now starts it here and now.
func startExtraEngines(ctx context.Context, cfg *config.HydraConfig, engineCfgs []config.EngineConfig,
	raceCfg, hoardCfg *config.SessionConfig, raceGate func(string) bool, front *api.Server) *extrasManager {

	m := newExtrasManager(ctx, cfg, raceGate, front)
	for i := range engineCfgs {
		ec := &engineCfgs[i]
		if &ec.SessionConfig == raceCfg || &ec.SessionConfig == hoardCfg {
			continue // primary engines are wired by main.go into api.Server
		}
		if err := m.spawn(*ec); err != nil {
			slog.Error("extra engine: start failed", "id", ec.ID, "error", err)
		}
	}
	return m
}

// startOneExtraEngine brings up a single extra engine: its Typhon process, the
// Go engine driving it, its per-engine store, and its tracker announcer. It is
// the whole of what "an engine runs here" means, in one place, so that a boot
// and a hot add produce the same thing rather than two subtly different ones.
//
// It does NOT publish the engine anywhere -- that is the manager's job -- so a
// failure part-way through leaves nothing half-registered for callers to find.
func startOneExtraEngine(ctx context.Context, cfg *config.HydraConfig, ec config.EngineConfig) (*liveEngine, error) {

	uploadsDir := filepath.Join(cfg.Daemon.DataDir, "uploads")
	eDir := engineDirFor(cfg, ec.ID)
	if err := os.MkdirAll(eDir, 0755); err != nil {
		return nil, fmt.Errorf("agent dir: %w", err)
	}
	sock := engineSocketFor(cfg, ec.ID)
	le := &liveEngine{id: ec.ID, role: ec.Role, cfg: ec.SessionConfig}
	proc, perr := engine.StartSessionEngine(&le.cfg, eDir, sock, ec.Role == "race")
	if perr != nil {
		return nil, perr
	}
	le.proc = proc
	le.ref = engine.NewEngineRef(proc.Client())
	if ec.Role == "race" {
		var ck engine.ChokingEngineInterface
		if le.cfg.CustomChoking != nil && le.cfg.CustomChoking.Enabled {
			ck = choking.NewChokingEngine(le.cfg.CustomChoking)
		}
		re := engine.NewRaceEngine(&le.cfg, ck, nil, eDir)
		re.SetClient(le.ref)
		le.race, le.rich = re, re
	} else {
		he := engine.NewHoardEngine(&le.cfg, eDir)
		he.SetClient(le.ref)
		le.hoard, le.rich = he, he
	}
	if st, serr := store.OpenAgent(filepath.Join(eDir, "store.db")); serr != nil {
		slog.Error("extra engine: open store", "id", ec.ID, "error", serr)
	} else {
		le.store = st
	}

	var serr error
	if le.race != nil {
		serr = le.race.Start(ctx)
	} else {
		serr = le.hoard.Start(ctx)
	}
	if serr != nil {
		if le.store != nil {
			le.store.Close()
		}
		proc.Stop()
		return nil, serr
	}

	if le.store != nil {
		var imp, errs int
		if le.race != nil {
			imp, errs = le.race.ImportFromStore(le.store, uploadsDir)
		} else {
			imp, errs = le.hoard.ImportFromStore(le.store, uploadsDir)
		}
		slog.Info("extra engine: store reload", "id", ec.ID, "imported", imp, "errors", errs)
	}
	slog.Info("extra engine: started", "id", ec.ID, "role", ec.Role, "port", le.cfg.ListenPort)
	return le, nil
}

// engineDirFor and engineSocketFor are where an engine's own state lives. Both
// a first start and a restart on new settings have to land on the same paths,
// or a reloaded engine would come back with an empty resume directory and its
// torrents would be gone.
func engineDirFor(cfg *config.HydraConfig, id string) string {
	return filepath.Join(cfg.Daemon.DataDir, id)
}

func engineSocketFor(cfg *config.HydraConfig, id string) string {
	return filepath.Join(cfg.Daemon.DataDir, id+".sock")
}

// stop tears one extra engine down, in the order that keeps the swarm honest:
// the announcer first (nothing should announce a swarm it is leaving), then the
// Go engine's goroutines, then a last store reconcile, then the process.
func (le *liveEngine) stop() {
	if le.ann != nil {
		le.ann.Stop()
	}
	if le.stopPump != nil {
		le.stopPump()
	}
	if le.race != nil {
		le.race.Stop()
	}
	if le.hoard != nil {
		le.hoard.Stop()
	}
	if le.store != nil {
		reconcileAgentStore(le.id, le.store, le.metas())
		le.store.Close()
	}
	if le.proc != nil {
		le.proc.Stop()
	}
}
