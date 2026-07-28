package store

import (
	"encoding/json"
	"fmt"
	"log/slog"
	"os"

	"github.com/Kheopsian/hydra/internal/state"
)

// ImportResult reports what a legacy backfill did.
type ImportResult struct {
	Imported    int // rows written (blob found on disk)
	MissingFile int // state entry whose .torrent file is gone — unrecoverable
	Errors      int // per-row failures
}

// ImportLegacy backfills the store from the legacy state.json plus the on-disk
// uploads/*.torrent blobs each entry points at. It runs in a single transaction
// so a 100k+ import is one commit, not 100k fsyncs. Idempotent: re-running
// upserts the same rows (info_hash PK), so it is safe to run before every boot
// until the store becomes the source of truth.
//
// Entries whose .torrent file is missing are counted (MissingFile) and skipped:
// without the metainfo blob there is nothing durable to keep, and those are
// exactly the torrents the old filename-collision bug had already orphaned.
func (s *Store) ImportLegacy(statePath string) (ImportResult, error) {
	var res ImportResult

	data, err := os.ReadFile(statePath)
	if err != nil {
		return res, fmt.Errorf("import: read state %s: %w", statePath, err)
	}
	var st state.State
	if err := json.Unmarshal(data, &st); err != nil {
		return res, fmt.Errorf("import: parse state: %w", err)
	}

	s.wmux.Lock()
	defer s.wmux.Unlock()

	tx, err := s.db.Begin()
	if err != nil {
		return res, fmt.Errorf("import: begin tx: %w", err)
	}
	defer tx.Rollback()

	stmt, err := tx.Prepare(`
        INSERT INTO torrents
            (info_hash, session, torrent, save_path, category,
             added_time, completed_time, total_uploaded, total_downloaded)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(info_hash) DO UPDATE SET
            session=excluded.session, torrent=excluded.torrent,
            save_path=excluded.save_path, category=excluded.category,
            added_time=excluded.added_time, completed_time=excluded.completed_time,
            total_uploaded=excluded.total_uploaded,
            total_downloaded=excluded.total_downloaded`)
	if err != nil {
		return res, fmt.Errorf("import: prepare: %w", err)
	}
	defer stmt.Close()

	imp := func(sess Session, metas map[string]*state.TorrentMeta) {
		for ih, m := range metas {
			if m == nil || m.TorrentFilePath == "" {
				res.MissingFile++
				continue
			}
			blob, err := os.ReadFile(m.TorrentFilePath)
			if err != nil {
				res.MissingFile++
				continue
			}
			if _, err := stmt.Exec(ih, string(sess), blob, m.SavePath, m.Category,
				m.AddedTime, m.CompletedTime, m.TotalUploaded, m.TotalDownloaded); err != nil {
				res.Errors++
				continue
			}
			res.Imported++
		}
	}
	imp(Hoard, st.HoardActive)
	imp(Race, st.Race)

	if err := tx.Commit(); err != nil {
		return res, fmt.Errorf("import: commit: %w", err)
	}
	slog.Info("store: legacy import complete",
		"imported", res.Imported, "missing_file", res.MissingFile, "errors", res.Errors)
	return res, nil
}
