package main

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"log/slog"
	"os"
	"path/filepath"
	"time"

	"github.com/Kheopsian/hydra/internal/agent"
	"github.com/Kheopsian/hydra/internal/choking"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/store"
)

// extraShardAddr is the loopback address the monolith serves its extra engines
// on, then dials itself to register them as the "local-shards" agent.
const extraShardAddr = "127.0.0.1:19190"

// startExtraEngines spawns every engine the monolith hosts BEYOND its primary
// race+hoard pair (Option A sharding: e.g. a second/third hoard to spread the
// per-torrent announce goroutine cost across processes). Each extra engine runs
// the same lifecycle as a dedicated agent engine — its own Typhon process,
// per-engine store, tracker announcer and periodic reconcile — and seeds
// independently. Returns the live engines so main can register + stop them.
//
// The primary race/hoard (already wired into api.Server) are identified by the
// SessionConfig pointers raceCfg/hoardCfg (which point into engineCfgs) and
// skipped. A legacy default.toml has no extras, so this is a no-op there.
func startExtraEngines(ctx context.Context, cfg *config.HydraConfig, engineCfgs []config.EngineConfig,
	raceCfg, hoardCfg *config.SessionConfig, raceGate func(string) bool) ([]*liveEngine, string, string) {

	uploadsDir := filepath.Join(cfg.Daemon.DataDir, "uploads")
	var lives []*liveEngine

	for i := range engineCfgs {
		ec := &engineCfgs[i]
		if &ec.SessionConfig == raceCfg || &ec.SessionConfig == hoardCfg {
			continue // primary engines are wired by main.go into api.Server
		}
		eDir := filepath.Join(cfg.Daemon.DataDir, ec.ID)
		_ = os.MkdirAll(eDir, 0755)
		sock := filepath.Join(cfg.Daemon.DataDir, ec.ID+".sock")
		le := &liveEngine{id: ec.ID, role: ec.Role, cfg: ec.SessionConfig}
		proc, perr := engine.StartSessionEngine(&le.cfg, eDir, sock, ec.Role == "race")
		if perr != nil {
			slog.Error("extra engine: start failed", "id", ec.ID, "error", perr)
			continue
		}
		le.proc = proc
		if ec.Role == "race" {
			var ck engine.ChokingEngineInterface
			if le.cfg.CustomChoking != nil && le.cfg.CustomChoking.Enabled {
				ck = choking.NewChokingEngine(le.cfg.CustomChoking)
			}
			re := engine.NewRaceEngine(&le.cfg, ck, nil, eDir)
			re.SetClient(proc.Client())
			le.race, le.rich = re, re
		} else {
			he := engine.NewHoardEngine(&le.cfg, eDir)
			he.SetClient(proc.Client())
			le.hoard, le.rich = he, he
		}
		if st, serr := store.OpenAgent(filepath.Join(eDir, "store.db")); serr != nil {
			slog.Error("extra engine: open store", "id", ec.ID, "error", serr)
		} else {
			le.store = st
		}

		if le.race != nil {
			if serr := le.race.Start(ctx); serr != nil {
				slog.Error("extra engine: race start", "id", ec.ID, "error", serr)
				proc.Stop()
				continue
			}
		} else {
			if serr := le.hoard.Start(ctx); serr != nil {
				slog.Error("extra engine: hoard start", "id", ec.ID, "error", serr)
				proc.Stop()
				continue
			}
		}

		ann := engine.NewHoardAnnouncer(proc.Client(), engine.ApplyAnnounceEgress(
			engine.DefaultSingleBinding(le.cfg.ListenPort, le.cfg.EnableIPv6, "hoard", le.cfg.AnnounceRateLimit),
			le.cfg.AnnounceProxy, le.cfg.AnnounceIP, le.cfg.Socks5OutboundHost, "hoard"))
		if le.hoard != nil {
			ann.OnObservation = le.hoard.ObserveAnnounce
			le.hoard.SetBootstrapAnnounce(ann.BootstrapAnnounce)
			le.hoard.SetReAnnounce(ann.ReAnnounce)
			ann.SetRaceGate(raceGate)
			ann.SetOffsetFn(le.hoard.AnnounceOffset)
		}
		ann.Start(ctx)
		le.ann = ann

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
		lives = append(lives, le)
	}

	if len(lives) == 0 {
		return lives, "", ""
	}
	go func() {
		t := time.NewTicker(5 * time.Minute)
		defer t.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-t.C:
				for _, le := range lives {
					reconcileAgentStore(le.id, le.store, le.metas())
				}
			}
		}
	}()

	// Serve the extras as an agent (ownEvents) so the front aggregates + routes
	// them via the same remote-agent path (main registers this loopback as the
	// "local-shards" agent). Reuses the whole multi-engine agent machinery.
	tok := make([]byte, 16)
	_, _ = rand.Read(tok)
	token := hex.EncodeToString(tok)
	engines := make(map[string]engine.EngineClient, len(lives))
	for _, le := range lives {
		engines[le.id] = le.proc.Client()
	}
	srv := agent.NewServer(engines, cfg.Daemon.DataDir, token)
	srv.SetOwnEvents(true)
	for _, le := range lives {
		srv.AddRichEngine(le.id, le.rich)
	}
	go func() {
		if serr := srv.Serve(ctx, extraShardAddr); serr != nil {
			slog.Error("extra engines: agent serve failed", "error", serr)
		}
	}()
	return lives, extraShardAddr, token
}
