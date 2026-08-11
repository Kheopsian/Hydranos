package main

import (
	"context"
	"fmt"
	"log/slog"
	"path/filepath"

	"github.com/Kheopsian/hydra/internal/api"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/fsinfo"
	"github.com/Kheopsian/hydra/internal/sqlitex"
)

// Coming up in store-repair mode.
//
// The databases Hydra keeps in data_dir are journalled with WAL, which is the
// right choice on a local disk and an impossible one on a share: WAL needs a
// shared-memory index file no SMB or NFS server can back. The share fallback
// therefore opens with nolock, and SQLite refuses nolock on a file whose header
// says WAL. Nothing about that is broken -- it is just unreachable by anything
// except moving an existing data_dir onto a share, which is a supported thing
// to do and used to end in a daemon that would not start.
//
// So we look before we open, and if that is what happened we start almost
// nothing: no store, no sidecar migration, no engines. Only enough API to
// explain it and offer the fix.

// databasesUnderRepair are the files sharing the fallback's fate. peers.db is
// absent on purpose: it does not go through the shared DSN and is already
// journalled the old way.
var databasesUnderRepair = []string{"hydra.db", "bench.db"}

// storeRepairDiagnosis asks, of each database, whether it can be opened where
// it now lives. A read error is reported and skipped rather than treated as a
// repair case: this decides whether Hydra starts at all, so it must only say
// yes when it is sure.
func storeRepairDiagnosis(dataDir string) *api.RepairState {
	st := &api.RepairState{}
	if _, kind := fsinfo.IsNetwork(dataDir); kind != "" {
		st.Filesystem = string(kind)
	}

	for _, name := range databasesUnderRepair {
		path := filepath.Join(dataDir, name)
		d, err := sqlitex.Diagnose(path)
		if err != nil {
			slog.Warn("store: could not inspect a database, leaving it alone",
				"database", name, "error", err)
			continue
		}
		if !d.NeedsRepair() {
			continue
		}
		st.Needed = true
		st.Targets = append(st.Targets, api.RepairTarget{
			Name:   name,
			Path:   path,
			HotWAL: d.HotWAL,
		})
	}
	return st
}

// logStoreRepairNeeded says it once, loudly, in terms that name the cause and
// the cure. The browser gets the same story with a button attached, but a
// headless operator reading logs deserves to understand it without one.
func logStoreRepairNeeded(st *api.RepairState) {
	names := make([]string, 0, len(st.Targets))
	hot := false
	for _, t := range st.Targets {
		names = append(names, t.Name)
		hot = hot || t.HotWAL
	}
	slog.Error("store: the database cannot be opened from where data_dir now points",
		"databases", fmt.Sprint(names), "filesystem", st.Filesystem)
	slog.Error("  it was created on local storage, so it uses a write-ahead log, and a share cannot host one")
	slog.Error("  Hydra has NOT started its engines: running without the store is what loses upload counters")
	slog.Error("  open the web interface to back the databases up and convert them, or move data_dir back to local disk")
	if hot {
		slog.Error("  one database still has unmerged changes in its log: start Hydra once on the machine it came from, stop it cleanly, then move it")
	}
}

// runStoreRepair serves the explanation and the fix, and nothing else. It
// blocks until the process is told to go, which in practice means the user
// pressed restart after a successful conversion.
func runStoreRepair(ctx context.Context, cfg *config.HydraConfig) {
	slog.Info("starting in STORE-REPAIR mode (engines stopped, API limited to the repair)")
	apiServer := api.NewServer(cfg)
	go func() {
		if err := apiServer.Run(); err != nil {
			slog.Error("API server error", "error", err)
		}
	}()
	slog.Info("repair interface ready",
		"addr", fmt.Sprintf("http://%s:%d", cfg.Daemon.APIHost, cfg.Daemon.APIPort))
	<-ctx.Done()
}
