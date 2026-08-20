package engine

import (
	"fmt"
	"log/slog"
)

// Pointing an engine at a payload that has moved.
//
// The mover does the bytes; this is the other half -- telling the engine where
// they went and keeping Hydra's own bookkeeping in step. It is deliberately
// the *only* thing the mover asks of an engine besides stop and start, which
// is what lets a move treat race and hoard identically.
//
// The files must already be at the new location. This is called after the swap
// and before the old copy is removed, so an error here is recoverable: the
// caller restarts the torrent and both copies still exist.

// SetEngineSavePath tells the engine the payload now lives at engineSavePath,
// and records the new on-disk root and category on the Go side.
//
// engineSavePath is Typhon's notion: the parent of the content root for a
// multi-file torrent, the content root itself for a single-file one.
// onDiskRoot is Hydra's notion: always the content root. Passing both beats
// re-deriving one from the other in two places and having them disagree.
func (e *HoardEngine) SetEngineSavePath(infoHash, engineSavePath, onDiskRoot, category string) error {
	if err := e.client.SetSavePath(infoHash, engineSavePath); err != nil {
		return fmt.Errorf("hoard: set engine save_path: %w", err)
	}
	e.mu.Lock()
	if inf, ok := e.torrents[infoHash]; ok {
		inf.SavePath = onDiskRoot
		if category != "" {
			inf.Category = category
		}
	}
	e.mu.Unlock()

	// The stats cache is otherwise only refreshed on the 60s tick, and until
	// then the qBit shim derives paths from it -- so a freshly moved torrent
	// would be reported at its old location to every *arr polling in that
	// window.
	e.cachedStatsMu.Lock()
	if st, ok := e.cachedStats[infoHash]; ok {
		st.SavePath = onDiskRoot
		st.EngineSavePath = engineSavePath
		if category != "" {
			st.Category = category
		}
	}
	e.cachedStatsMu.Unlock()
	slog.Info("hoard: payload relocated", "info_hash", infoHash, "save_path", onDiskRoot)
	return nil
}

func (e *RaceEngine) SetEngineSavePath(infoHash, engineSavePath, onDiskRoot, category string) error {
	if err := e.client.SetSavePath(infoHash, engineSavePath); err != nil {
		return fmt.Errorf("race: set engine save_path: %w", err)
	}
	e.mu.Lock()
	if inf, ok := e.torrents[infoHash]; ok {
		inf.SavePath = onDiskRoot
		if category != "" {
			inf.Category = category
		}
	}
	e.mu.Unlock()
	slog.Info("race: payload relocated", "info_hash", infoHash, "save_path", onDiskRoot)
	return nil
}
