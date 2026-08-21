package engine

import (
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/Kheopsian/hydra/internal/store"
)

// This file lets a race/hoard engine boot from its own per-agent store
// (internal/store.AgentStore) instead of state.json + uploads/. Identity,
// placement and the .torrent blob all come from the DB, so a restart can never
// silently drop a torrent because a filename was reused. Additive: nothing calls
// these until the agent boot path (Phase 4) / --boot-from-store (Phase 6) wires
// them, so the monolith prod path is unchanged.

func storeCompletedTime(sec float64) time.Time {
	if sec <= 0 {
		return time.Time{}
	}
	return time.Unix(int64(sec), 0)
}

// materializeBlob writes a content-addressed .torrent (<uploadsDir>/<ih>.torrent)
// so the engine gets a stable, persistent TorrentFilePath. Content-addressing by
// info_hash means a filename can never be reused — the exact defect behind the
// datafarm loss. Idempotent: skips the write if the file already exists.
func materializeBlob(uploadsDir, infoHash string, blob []byte) (string, error) {
	if err := os.MkdirAll(uploadsDir, 0755); err != nil {
		return "", err
	}
	path := filepath.Join(uploadsDir, infoHash+".torrent")
	if _, err := os.Stat(path); err != nil {
		if werr := os.WriteFile(path, blob, 0644); werr != nil {
			return "", werr
		}
	}
	return path, nil
}

// benignAddErr reports whether an add error just means the torrent is already
// present — either the store had a dup, or (the common case at boot) the engine
// already reloaded it from its own resume/ data before this import ran. Both are
// fine: we still apply the store's authoritative metadata and count it.
func benignAddErr(err error) bool {
	m := err.Error()
	return strings.Contains(m, "duplicate") || strings.Contains(m, "already added")
}

// ImportFromStore reloads the hoard engine from its per-agent store. Mirrors
// ImportFromState's stagger + duplicate handling.
func (e *HoardEngine) ImportFromStore(st *store.AgentStore, uploadsDir string) (imported, errors int) {
	recs, err := st.All()
	if err != nil {
		slog.Error("hoard: store import list failed", "error", err)
		return
	}
	total := len(recs)
	// Publish the true total so total_torrents shows the real count during the
	// multi-minute import instead of climbing from 0 as the map fills.
	e.expectedTotal.Store(int64(total))
	defer e.expectedTotal.Store(0)
	// The engine already reloaded its torrents from resume/ at startup. For
	// those we only need to (re)populate the Go bookkeeping map e.torrents — no
	// per-torrent add RPC. One bulk ListTorrents gives the engine's set + each
	// name; anything the engine is missing (store-only delta) still gets a real
	// add. If ListTorrents fails, nameByHash stays empty -> every rec falls back
	// to the old per-torrent add path (safe).
	nameByHash := map[string]string{}
	if res, lerr := e.client.ListTorrents(); lerr == nil && res != nil {
		for i := range res.Torrents {
			nameByHash[res.Torrents[i].InfoHash] = res.Torrents[i].Name
		}
	} else if lerr != nil {
		slog.Warn("hoard: store import, ListTorrents failed, falling back to per-torrent add", "error", lerr)
	}
	slog.Info("hoard: importing from store", "total", total, "engine_has", len(nameByHash))

	fast := make(map[string]*TorrentInfo, len(recs))
	realAdds := 0
	for _, rec := range recs {
		path, merr := materializeBlob(uploadsDir, rec.InfoHash, rec.Torrent)
		if merr != nil {
			errors++
			continue
		}
		ct := storeCompletedTime(rec.CompletedTime)
		if name, ok := nameByHash[rec.InfoHash]; ok {
			// Engine already has it — build the bookkeeping entry locally (no RPC).
			at := storeCompletedTime(rec.AddedTime)
			if at.IsZero() {
				at = time.Now()
			}
			fast[rec.InfoHash] = &TorrentInfo{
				InfoHash:        rec.InfoHash,
				Name:            name,
				SavePath:        rec.SavePath,
				Category:        rec.Category,
				AddedTime:       at,
				TorrentFilePath: path,
				CompletedTime:   ct,
			}
			imported++
			continue
		}
		// Delta: in store, not (yet) in engine -> real add (throttled like before).
		realAdds++
		if realAdds%200 == 0 {
			time.Sleep(300 * time.Millisecond)
		}
		if _, aerr := e.AddTorrentSeedMode(path, rec.SavePath, rec.Category); aerr != nil {
			if !benignAddErr(aerr) {
				errors++
				if errors <= 5 {
					slog.Warn("hoard: store import error", "info_hash", rec.InfoHash, "error", aerr)
				}
				continue
			}
		}
		e.RestoreMetadata(rec.InfoHash, rec.Category, rec.SavePath, path, ct)
		imported++
	}
	if len(fast) > 0 {
		e.mu.Lock()
		for ih, info := range fast {
			if existing, exists := e.torrents[ih]; exists {
				// The engine rebuilt this torrent from its own resume data,
				// which carries no category and no save path -- those live in
				// the store now. Dropping the store record here was what made a
				// restored category disappear again: the next sync writes the
				// engine's view back over the store, blanking what it holds.
				adoptStoreMetadata(existing, info)
				continue
			}
			e.torrents[ih] = info
		}
		e.mu.Unlock()
	}
	slog.Info("hoard: store import complete", "imported", imported, "errors", errors, "fast", len(fast), "real_adds", realAdds)
	return
}

// ImportFromStoreSession reloads the monolith's <sess> engine from the
// session-keyed store.Store (the shadow DB kept current by saveState). Same
// content-addressed materialization as ImportFromStore, so a reused .torrent
// filename can never silently drop a torrent. Used by --boot-from-store.
func (e *HoardEngine) ImportFromStoreSession(st *store.Store, sess store.Session, uploadsDir string) (imported, errors int) {
	recs, err := st.BySession(sess)
	if err != nil {
		slog.Error("hoard: store-session import list failed", "error", err)
		return
	}
	total := len(recs)
	e.expectedTotal.Store(int64(total))
	defer e.expectedTotal.Store(0)

	// Fast path (C): the engine already reloaded its torrents from resume/ at
	// startup, so re-adding them one-by-one over IPC was pure redundancy. One
	// bulk ListTorrents -> populate e.torrents locally for the engine-present
	// majority (no RPC); only a store-only delta gets a real add. Empty
	// nameByHash (ListTorrents failed) falls back to the old per-torrent path.
	nameByHash := map[string]string{}
	if res, lerr := e.client.ListTorrents(); lerr == nil && res != nil {
		for i := range res.Torrents {
			nameByHash[res.Torrents[i].InfoHash] = res.Torrents[i].Name
		}
	} else if lerr != nil {
		slog.Warn("hoard: store-session import, ListTorrents failed, per-torrent fallback", "error", lerr)
	}
	slog.Info("hoard: importing from store (session)", "total", total, "engine_has", len(nameByHash))

	fast := make(map[string]*TorrentInfo, len(recs))
	realAdds := 0
	for _, rec := range recs {
		path, merr := materializeBlob(uploadsDir, rec.InfoHash, rec.Torrent)
		if merr != nil {
			errors++
			continue
		}
		ct := storeCompletedTime(rec.CompletedTime)
		if name, ok := nameByHash[rec.InfoHash]; ok {
			fast[rec.InfoHash] = &TorrentInfo{
				ContentFolder:   rec.ContentFolder,
				InfoHash:        rec.InfoHash,
				Name:            name,
				SavePath:        rec.SavePath,
				Category:        rec.Category,
				AddedTime:       time.Now(),
				TorrentFilePath: path,
				CompletedTime:   ct,
			}
			imported++
			continue
		}
		realAdds++
		if realAdds%200 == 0 {
			time.Sleep(300 * time.Millisecond)
		}
		if _, aerr := e.AddTorrentSeedMode(path, rec.SavePath, rec.Category); aerr != nil {
			if !benignAddErr(aerr) {
				errors++
				if errors <= 5 {
					slog.Warn("hoard: store import error", "info_hash", rec.InfoHash, "error", aerr)
				}
				continue
			}
		}
		e.RestoreMetadata(rec.InfoHash, rec.Category, rec.SavePath, path, ct)
		// nil means the row predates the flag: leave the engine on its
		// legacy layout instead of asserting "no wrapper folder".
		if rec.ContentFolder != nil {
			e.SetContentFolder(rec.InfoHash, rec.ContentFolder)
		}
		imported++
	}
	if len(fast) > 0 {
		e.mu.Lock()
		for ih, info := range fast {
			if existing, exists := e.torrents[ih]; exists {
				// The engine rebuilt this torrent from its own resume data,
				// which carries no category and no save path -- those live in
				// the store now. Dropping the store record here was what made a
				// restored category disappear again: the next sync writes the
				// engine's view back over the store, blanking what it holds.
				adoptStoreMetadata(existing, info)
				continue
			}
			e.torrents[ih] = info
		}
		e.mu.Unlock()
	}
	slog.Info("hoard: store-session import complete", "imported", imported, "errors", errors, "fast", len(fast), "real_adds", realAdds)
	return
}

// adoptStoreMetadata copies the durable, store-owned fields onto an entry the
// engine had already rebuilt from resume data.
//
// A field the store does not carry is left alone: the store fills gaps, it
// never blanks what the engine knows. That matters while a database is still
// catching up -- an empty category in the store is not yet a statement that the
// torrent has none, only that the store was never told.
func adoptStoreMetadata(dst, src *TorrentInfo) {
	if src.Category != "" {
		dst.Category = src.Category
	}
	if src.SavePath != "" {
		dst.SavePath = src.SavePath
	}
	if src.TorrentFilePath != "" {
		dst.TorrentFilePath = src.TorrentFilePath
	}
	if !src.CompletedTime.IsZero() {
		dst.CompletedTime = src.CompletedTime
	}
	if src.ContentFolder != nil {
		dst.ContentFolder = src.ContentFolder
	}
}

// ImportFromStoreSession reloads the monolith's race engine from the store.
func (e *RaceEngine) ImportFromStoreSession(st *store.Store, sess store.Session, uploadsDir string) (imported, errors int) {
	recs, err := st.BySession(sess)
	if err != nil {
		slog.Error("race: store-session import list failed", "error", err)
		return
	}
	total := len(recs)
	slog.Info("race: importing from store (session)", "total", total)
	for i, rec := range recs {
		if (i+1)%200 == 0 {
			slog.Info("race: store import progress", "done", i+1, "total", total, "imported", imported, "errors", errors)
			time.Sleep(300 * time.Millisecond)
		}
		path, merr := materializeBlob(uploadsDir, rec.InfoHash, rec.Torrent)
		if merr != nil {
			errors++
			continue
		}
		ct := storeCompletedTime(rec.CompletedTime)
		if _, aerr := e.AddTorrentSeedMode(path, rec.SavePath, rec.Category); aerr != nil {
			if !benignAddErr(aerr) {
				errors++
				if errors <= 5 {
					slog.Warn("race: store import error", "info_hash", rec.InfoHash, "error", aerr)
				}
				continue
			}
		}
		e.RestoreMetadata(rec.InfoHash, rec.Category, rec.SavePath, path, ct)
		imported++
	}
	slog.Info("race: store-session import complete", "imported", imported, "errors", errors)
	return
}

// ImportFromStore reloads the race engine from its per-agent store.
func (e *RaceEngine) ImportFromStore(st *store.AgentStore, uploadsDir string) (imported, errors int) {
	recs, err := st.All()
	if err != nil {
		slog.Error("race: store import list failed", "error", err)
		return
	}
	total := len(recs)
	slog.Info("race: importing from store", "total", total)
	for i, rec := range recs {
		if (i+1)%200 == 0 {
			slog.Info("race: store import progress", "done", i+1, "total", total, "imported", imported, "errors", errors)
			time.Sleep(300 * time.Millisecond)
		}
		path, merr := materializeBlob(uploadsDir, rec.InfoHash, rec.Torrent)
		if merr != nil {
			errors++
			continue
		}
		ct := storeCompletedTime(rec.CompletedTime)
		if _, aerr := e.AddTorrentSeedMode(path, rec.SavePath, rec.Category); aerr != nil {
			if !benignAddErr(aerr) {
				errors++
				if errors <= 5 {
					slog.Warn("race: store import error", "info_hash", rec.InfoHash, "error", aerr)
				}
				continue
			}
		}
		e.RestoreMetadata(rec.InfoHash, rec.Category, rec.SavePath, path, ct)
		imported++
	}
	slog.Info("race: store import complete", "imported", imported, "errors", errors)
	return
}

// MaterializeTorrentBlob writes shipped .torrent bytes to the durable,
// content-addressed path every other durability path already agrees on
// (<uploadsDir>/<info_hash>.torrent) and returns it. Exported for the agent's
// routed add: an engine keeps the path it was added from as its
// TorrentFilePath, so adding from a temp file that is deleted right after
// leaves the torrent with a dangling path, and the store reconcile — which
// captures the blob by reading that path — can never insert its row.
func MaterializeTorrentBlob(uploadsDir string, blob []byte) (string, error) {
	ih, err := infoHashFromTorrentFile(blob)
	if err != nil {
		return "", err
	}
	return materializeBlob(uploadsDir, ih, blob)
}
