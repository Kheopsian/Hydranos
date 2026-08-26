package main

import (
	"log/slog"
	"os"

	"github.com/Kheopsian/hydra/internal/config"
)

// foldSidecars folds engines.json into the [[agent]] array, once, at boot.
//
// Three files described the same thing for different readers: engines.json for
// engines the UI created, agents.json for nodes it dialled, the TOML for
// everything a human wrote -- and the Network tab wrote to a fourth place.
// Which one won depended on the code path. config.MigrateSidecars has done the
// rewriting since it was written; nothing ever called it, so the sidecar stayed
// the live source and the array stayed a plan.
//
// Additive and reversible: nothing is deleted from the TOML, the previous file
// is kept as .bak-migrate, and the sidecar is renamed rather than removed. An
// install that loses its engines is a far worse outcome than a leftover file.
//
// Returns the config to run with -- the reloaded one on success, the one it was
// given on any failure, since a node that boots on a stale config still seeds
// while a node that refuses to boot does not.
func foldSidecars(cfg *config.HydraConfig) *config.HydraConfig {
	path := cfg.SourcePath
	if path == "" {
		return cfg
	}
	data, err := os.ReadFile(path)
	if err != nil {
		slog.Warn("sidecar fold: config unreadable, leaving engines.json as the source", "error", err)
		return cfg
	}
	doc, done, err := config.MigrateSidecars(string(data), cfg.Daemon.DataDir)
	if err != nil {
		slog.Error("sidecar fold: failed, engines.json stays the source", "error", err)
		return cfg
	}
	if len(done) == 0 {
		return cfg
	}
	// Same guards as the settings editor. A migration that produces a document
	// the daemon cannot parse would take the node down at its next boot, long
	// after the change that caused it.
	if _, perr := config.ParseTOMLMap([]byte(doc)); perr != nil {
		slog.Error("sidecar fold: the result no longer parses, keeping the old config", "error", perr)
		return cfg
	}
	if verr := config.ValidateTyped([]byte(doc)); verr != nil {
		slog.Error("sidecar fold: the result breaks the config schema, keeping the old config", "error", verr)
		return cfg
	}
	_ = os.WriteFile(path+".bak-migrate", data, 0644)
	tmp := path + ".tmp"
	if werr := os.WriteFile(tmp, []byte(doc), 0644); werr != nil {
		slog.Error("sidecar fold: could not write the migrated config", "error", werr)
		return cfg
	}
	if rerr := os.Rename(tmp, path); rerr != nil {
		slog.Error("sidecar fold: could not install the migrated config", "error", rerr)
		return cfg
	}
	// Only now: while the sidecar is still in place, a crash between the two
	// leaves the engines described twice, which converges. Renaming it first
	// and then failing to write would lose them.
	if merr := config.MarkSidecarMigrated(cfg.Daemon.DataDir, "engines.json"); merr != nil {
		slog.Warn("sidecar fold: engines.json could not be renamed aside", "error", merr)
	}
	reloaded, rerr := config.Reload(path)
	if rerr != nil {
		slog.Error("sidecar fold: the migrated config did not reload", "error", rerr)
		return cfg
	}
	for _, d := range done {
		slog.Info("sidecar fold", "moved", d)
	}
	return reloaded
}
