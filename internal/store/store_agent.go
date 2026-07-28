package store

import (
	"database/sql"
	"fmt"
	"os"
	"sync"

	_ "modernc.org/sqlite"
)

// AgentStore is the per-agent variant of the torrent identity store: one DB per
// engine, so the DB *is* the agent. There is no `session` column — a race agent
// and a hoard agent are two separate files (race.db / hoard.db), which makes a
// dual-seeded info_hash (same hash in both) a non-event instead of a PK clash,
// and lets a whole engine be relocated to another host by copying one folder.
//
// This is the shape the agentified world uses (owned by internal/agent). The
// session-aware Store in store.go stays for the monolith shadow until the boot
// path flips to booting each engine from its own AgentStore.
type AgentStore struct {
	db   *sql.DB
	wmux sync.Mutex
}

// AgentRecord is the durable identity of a torrent within one agent's engine.
type AgentRecord struct {
	InfoHash        string
	Torrent         []byte
	SavePath        string
	Category        string
	AddedTime       float64
	CompletedTime   float64
	TotalUploaded   int64
	TotalDownloaded int64
}

const schemaAgent = `
CREATE TABLE IF NOT EXISTS torrents (
    info_hash        TEXT PRIMARY KEY,
    torrent          BLOB NOT NULL,
    save_path        TEXT NOT NULL DEFAULT '',
    category         TEXT NOT NULL DEFAULT '',
    added_time       REAL NOT NULL DEFAULT 0,
    completed_time   REAL NOT NULL DEFAULT 0,
    total_uploaded   INTEGER NOT NULL DEFAULT 0,
    total_downloaded INTEGER NOT NULL DEFAULT 0
);
`

// OpenAgent opens (creating if needed) a per-agent store at path.
func OpenAgent(path string) (*AgentStore, error) {
	dsn := "file:" + path +
		"?_pragma=journal_mode(WAL)" +
		"&_pragma=synchronous(NORMAL)" +
		"&_pragma=busy_timeout(5000)"
	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, fmt.Errorf("open agent store: %w", err)
	}
	if _, err := db.Exec(schemaAgent); err != nil {
		db.Close()
		return nil, fmt.Errorf("init agent schema: %w", err)
	}
	return &AgentStore{db: db}, nil
}

// Close closes the underlying database.
func (s *AgentStore) Close() error { return s.db.Close() }

// Put durably upserts a torrent's full identity (blob + metadata), keyed on
// info_hash — collisions are impossible by construction.
func (s *AgentStore) Put(r *AgentRecord) error {
	if r.InfoHash == "" {
		return fmt.Errorf("agent store: empty info_hash")
	}
	if len(r.Torrent) == 0 {
		return fmt.Errorf("agent store: empty torrent blob for %s", r.InfoHash)
	}
	s.wmux.Lock()
	defer s.wmux.Unlock()
	_, err := s.db.Exec(`
        INSERT INTO torrents
            (info_hash, torrent, save_path, category, added_time, completed_time, total_uploaded, total_downloaded)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(info_hash) DO UPDATE SET
            torrent=excluded.torrent, save_path=excluded.save_path, category=excluded.category,
            added_time=excluded.added_time, completed_time=excluded.completed_time,
            total_uploaded=excluded.total_uploaded, total_downloaded=excluded.total_downloaded`,
		r.InfoHash, r.Torrent, r.SavePath, r.Category, r.AddedTime, r.CompletedTime, r.TotalUploaded, r.TotalDownloaded)
	if err != nil {
		return fmt.Errorf("agent store put %s: %w", r.InfoHash, err)
	}
	return nil
}

// UpdateStats cheaply refreshes counters/completion without touching the blob.
func (s *AgentStore) UpdateStats(infoHash string, completedTime float64, up, down int64) error {
	s.wmux.Lock()
	defer s.wmux.Unlock()
	_, err := s.db.Exec(`UPDATE torrents SET completed_time=?, total_uploaded=?, total_downloaded=? WHERE info_hash=?`,
		completedTime, up, down, infoHash)
	return err
}

// UpdatePlacement updates category/save_path (a move) without touching the blob.
func (s *AgentStore) UpdatePlacement(infoHash, category, savePath string) error {
	s.wmux.Lock()
	defer s.wmux.Unlock()
	_, err := s.db.Exec(`UPDATE torrents SET category=?, save_path=? WHERE info_hash=?`, category, savePath, infoHash)
	return err
}

// Delete removes a torrent's identity (never touches payload files on disk).
func (s *AgentStore) Delete(infoHash string) error {
	s.wmux.Lock()
	defer s.wmux.Unlock()
	_, err := s.db.Exec(`DELETE FROM torrents WHERE info_hash=?`, infoHash)
	return err
}

// Get returns a single record; ok is false if absent.
func (s *AgentStore) Get(infoHash string) (*AgentRecord, bool, error) {
	row := s.db.QueryRow(`
        SELECT info_hash, torrent, save_path, category, added_time, completed_time, total_uploaded, total_downloaded
        FROM torrents WHERE info_hash=?`, infoHash)
	r, err := scanAgent(row)
	if err == sql.ErrNoRows {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, err
	}
	return r, true, nil
}

// All returns every stored record (used to reload the engine at boot).
func (s *AgentStore) All() ([]*AgentRecord, error) {
	rows, err := s.db.Query(`
        SELECT info_hash, torrent, save_path, category, added_time, completed_time, total_uploaded, total_downloaded
        FROM torrents`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []*AgentRecord
	for rows.Next() {
		r, err := scanAgent(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, r)
	}
	return out, rows.Err()
}

// Count returns the number of stored torrents.
func (s *AgentStore) Count() (int, error) {
	var n int
	err := s.db.QueryRow(`SELECT COUNT(*) FROM torrents`).Scan(&n)
	return n, err
}

// AgentSyncItem is one engine-reported torrent for reconcile.
type AgentSyncItem struct {
	InfoHash        string
	SavePath        string
	Category        string
	TorrentFilePath string // read once to capture the blob on first insert
	CompletedTime   float64
}

// Reconcile brings the store in line with the engine's current torrent list in
// one transaction: new torrents inserted (blob read once), known ones refreshed,
// gone ones deleted. Refuses to delete anything on an empty report (transient
// engine hiccup guard). One engine == one DB, so no cross-session logic.
func (s *AgentStore) Reconcile(items []AgentSyncItem) (SyncResult, error) {
	var res SyncResult
	s.wmux.Lock()
	defer s.wmux.Unlock()

	existing := map[string]bool{}
	rows, err := s.db.Query(`SELECT info_hash FROM torrents`)
	if err != nil {
		return res, fmt.Errorf("reconcile: query: %w", err)
	}
	for rows.Next() {
		var ih string
		if err := rows.Scan(&ih); err != nil {
			rows.Close()
			return res, err
		}
		existing[ih] = true
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return res, err
	}

	desired := make(map[string]AgentSyncItem, len(items))
	for _, it := range items {
		if it.InfoHash != "" {
			desired[it.InfoHash] = it
		}
	}
	if len(desired) == 0 && len(existing) > 0 {
		return res, fmt.Errorf("reconcile: refusing to wipe %d rows on empty report", len(existing))
	}

	tx, err := s.db.Begin()
	if err != nil {
		return res, err
	}
	defer tx.Rollback()
	ins, err := tx.Prepare(`INSERT INTO torrents (info_hash, torrent, save_path, category, completed_time) VALUES (?, ?, ?, ?, ?)`)
	if err != nil {
		return res, err
	}
	defer ins.Close()
	upd, err := tx.Prepare(`UPDATE torrents SET save_path=?, category=?, completed_time=? WHERE info_hash=?`)
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
		if existing[ih] {
			if _, err := upd.Exec(it.SavePath, it.Category, it.CompletedTime, ih); err != nil {
				return res, fmt.Errorf("reconcile: update %s: %w", ih, err)
			}
			res.Updated++
			continue
		}
		blob, err := os.ReadFile(it.TorrentFilePath)
		if err != nil || len(blob) == 0 {
			res.Missing++
			continue
		}
		if _, err := ins.Exec(ih, blob, it.SavePath, it.Category, it.CompletedTime); err != nil {
			return res, fmt.Errorf("reconcile: insert %s: %w", ih, err)
		}
		res.Inserted++
	}
	for ih := range existing {
		if _, want := desired[ih]; !want {
			if _, err := del.Exec(ih); err != nil {
				return res, fmt.Errorf("reconcile: delete %s: %w", ih, err)
			}
			res.Deleted++
		}
	}
	if err := tx.Commit(); err != nil {
		return res, fmt.Errorf("reconcile: commit: %w", err)
	}
	return res, nil
}

func scanAgent(sc scanner) (*AgentRecord, error) {
	var r AgentRecord
	if err := sc.Scan(&r.InfoHash, &r.Torrent, &r.SavePath, &r.Category,
		&r.AddedTime, &r.CompletedTime, &r.TotalUploaded, &r.TotalDownloaded); err != nil {
		return nil, err
	}
	return &r, nil
}

// SplitLegacyDB migrates the monolith's session-keyed store.go DB into two
// per-agent stores (race + hoard), dropping the session column. One-shot and
// idempotent (upsert). Used at the boot-from-store flip if per-agent DBs are
// absent. srcPath is opened read-only; dst stores must already be open.
func SplitLegacyDB(srcPath string, race, hoard *AgentStore) (raceN, hoardN int, err error) {
	src, err := sql.Open("sqlite", "file:"+srcPath+"?mode=ro&_pragma=busy_timeout(5000)")
	if err != nil {
		return 0, 0, fmt.Errorf("split: open src: %w", err)
	}
	defer src.Close()

	rows, err := src.Query(`
        SELECT info_hash, session, torrent, save_path, category, added_time, completed_time, total_uploaded, total_downloaded
        FROM torrents`)
	if err != nil {
		return 0, 0, fmt.Errorf("split: query src: %w", err)
	}
	defer rows.Close()

	for rows.Next() {
		var r AgentRecord
		var sess string
		if err := rows.Scan(&r.InfoHash, &sess, &r.Torrent, &r.SavePath, &r.Category,
			&r.AddedTime, &r.CompletedTime, &r.TotalUploaded, &r.TotalDownloaded); err != nil {
			return raceN, hoardN, err
		}
		dst := hoard
		if Session(sess) == Race {
			dst = race
		}
		if err := dst.Put(&r); err != nil {
			return raceN, hoardN, err
		}
		if dst == race {
			raceN++
		} else {
			hoardN++
		}
	}
	return raceN, hoardN, rows.Err()
}
