package store

import (
	"fmt"
	"os"
)

// SyncItem is the engine's view of a torrent, fed to the store to reconcile.
type SyncItem struct {
	InfoHash        string
	Session         Session
	SavePath        string
	Category        string
	TorrentFilePath string // read once to capture the blob on first insert
	CompletedTime   float64
	Paused          bool     // the user's pause intent, carried from the engine
	Tags            []string // carried for the same reason as Paused
	ContentFolder   *bool    // nil = added before the flag existed (legacy layout)
}

// SyncResult reports what a reconcile did.
type SyncResult struct {
	Inserted  int
	Updated   int
	Deleted   int
	Missing   int // new item whose .torrent blob was unreadable — skipped
	Conflicts int // info_hash reported by more than one session (last wins)
}

// SyncAll reconciles the WHOLE store against both engines in one transaction.
//
// It is global on purpose: the primary key is info_hash alone, so a per-session
// reconcile collides whenever a hash is reported by more than one engine (or
// moved between them). Here existence is checked globally and rows are upserted,
// so a cross-session hash simply updates its row (last session wins, counted in
// Conflicts) instead of failing an INSERT.
//
// Wipe safety is preserved per session: an engine that reports zero items has
// its existing rows left untouched (never deleted), guarding against a transient
// engine hiccup (e.g. mid-restart) wiping a whole session. Callers treat store
// errors as best-effort and never fail the real operation.
func (s *Store) SyncAll(items []SyncItem) (SyncResult, error) {
	var res SyncResult

	s.wmux.Lock()
	defer s.wmux.Unlock()

	// Desired state, deduped by info_hash (last wins), plus per-session counts.
	desired := make(map[string]SyncItem, len(items))
	desiredPerSession := map[Session]int{}
	for _, it := range items {
		if it.InfoHash == "" {
			continue
		}
		if _, dup := desired[it.InfoHash]; dup {
			res.Conflicts++
		}
		desired[it.InfoHash] = it
	}
	for _, it := range desired {
		desiredPerSession[it.Session]++
	}

	// Existing rows (global): info_hash -> session.
	existing := map[string]Session{}
	rows, err := s.db.Query(`SELECT info_hash, session FROM torrents`)
	if err != nil {
		return res, fmt.Errorf("sync: query existing: %w", err)
	}
	for rows.Next() {
		var ih, sess string
		if err := rows.Scan(&ih, &sess); err != nil {
			rows.Close()
			return res, err
		}
		existing[ih] = Session(sess)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return res, err
	}

	if len(desired) == 0 && len(existing) > 0 {
		return res, fmt.Errorf("sync: refusing to wipe %d rows on empty engine report", len(existing))
	}

	tx, err := s.db.Begin()
	if err != nil {
		return res, fmt.Errorf("sync: begin: %w", err)
	}
	defer tx.Rollback()

	ins, err := tx.Prepare(`
        INSERT INTO torrents (info_hash, session, torrent, save_path, category, completed_time, paused, tags, content_folder)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`)
	if err != nil {
		return res, err
	}
	defer ins.Close()
	upd, err := tx.Prepare(`
        UPDATE torrents SET session=?, save_path=?, category=?, completed_time=?, paused=?, tags=?, content_folder=? WHERE info_hash=?`)
	if err != nil {
		return res, err
	}
	defer upd.Close()
	del, err := tx.Prepare(`DELETE FROM torrents WHERE info_hash=?`)
	if err != nil {
		return res, err
	}
	defer del.Close()

	for ih, it := range desired {
		if _, ok := existing[ih]; ok {
			if _, err := upd.Exec(string(it.Session), it.SavePath, it.Category, it.CompletedTime, boolToInt(it.Paused), encodeTags(it.Tags), contentFolderInt(it.ContentFolder), ih); err != nil {
				return res, fmt.Errorf("sync: update %s: %w", ih, err)
			}
			res.Updated++
			continue
		}
		blob, err := os.ReadFile(it.TorrentFilePath)
		if err != nil || len(blob) == 0 {
			res.Missing++
			continue
		}
		if _, err := ins.Exec(ih, string(it.Session), blob, it.SavePath, it.Category, it.CompletedTime, boolToInt(it.Paused), encodeTags(it.Tags), contentFolderInt(it.ContentFolder)); err != nil {
			return res, fmt.Errorf("sync: insert %s: %w", ih, err)
		}
		res.Inserted++
	}

	// Delete rows the engine no longer holds — but only for sessions that
	// actually reported something this cycle (per-session hiccup guard).
	for ih, sess := range existing {
		if _, want := desired[ih]; want {
			continue
		}
		if desiredPerSession[sess] == 0 {
			continue // that session reported nothing — do not wipe it
		}
		if _, err := del.Exec(ih); err != nil {
			return res, fmt.Errorf("sync: delete %s: %w", ih, err)
		}
		res.Deleted++
	}

	if err := tx.Commit(); err != nil {
		return res, fmt.Errorf("sync: commit: %w", err)
	}
	return res, nil
}
