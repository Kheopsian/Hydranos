// Package store is the durable, restart-safe home for a torrent's identity:
// the .torrent metainfo blob plus its placement (session, category, save_path)
// and lifetime counters. It replaces the two lossy legacy mechanisms:
//
//   - uploads/<name>.torrent  — collided when callers reused a filename, so a
//     restart silently dropped every torrent whose on-disk .torrent had been
//     overwritten (the datafarm loss).
//   - state.json              — a single monolithic map rewritten in full on
//     every change, too expensive at 100k+ to persist per-add, hence batched
//     and lossy across a crash.
//
// Here info_hash is the primary key, so collisions are impossible by
// construction, and each add is a single durable transaction. The engine keeps
// owning fast-resume (download progress) on disk: losing recent resume only
// costs a re-verify, never a lost torrent. This store owns identity; the engine
// owns progress.
package store

import (
	"database/sql"
	"fmt"
	"sync"

	_ "modernc.org/sqlite"
)

// Session names the engine a torrent belongs to.
type Session string

const (
	Race  Session = "race"
	Hoard Session = "hoard"
)

// Record is the durable identity of a torrent.
type Record struct {
	InfoHash        string
	Session         Session
	Torrent         []byte // the raw .torrent metainfo
	SavePath        string
	Category        string
	AddedTime       float64
	CompletedTime   float64
	TotalUploaded   int64
	TotalDownloaded int64
	// ContentFolder says whether the payload sits in its own folder. nil means
	// "added before the flag existed", which the engine reads as the legacy
	// wrapped layout -- distinct from an explicit false.
	ContentFolder *bool
	// Paused is the user's intent, not the scheduler's: a torrent held back by
	// the download or disk slot managers is not paused, it is queued. Only a
	// human (or an API call made on their behalf) sets this, and nothing
	// automatic clears it — that is what makes it survive a restart meaningfully.
	Paused bool
	Tags   []string
}

// Store is a SQLite-backed torrent identity store. Safe for concurrent use.
type Store struct {
	db   *sql.DB
	wmux sync.Mutex // serialize writers; SQLite allows one writer at a time
}

const schema = `
CREATE TABLE IF NOT EXISTS torrents (
    info_hash        TEXT PRIMARY KEY,
    session          TEXT NOT NULL,
    torrent          BLOB NOT NULL,
    save_path        TEXT NOT NULL DEFAULT '',
    category         TEXT NOT NULL DEFAULT '',
    added_time       REAL NOT NULL DEFAULT 0,
    completed_time   REAL NOT NULL DEFAULT 0,
    total_uploaded   INTEGER NOT NULL DEFAULT 0,
    total_downloaded INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_torrents_session ON torrents(session);
`

// Open opens (creating if needed) the store at path. WAL + a busy timeout are
// set on every pooled connection via the DSN so readers never block the writer.
func Open(path string) (*Store, error) {
	dsn := "file:" + path +
		"?_pragma=journal_mode(WAL)" +
		"&_pragma=synchronous(NORMAL)" +
		"&_pragma=busy_timeout(5000)" +
		"&_pragma=foreign_keys(ON)"
	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, fmt.Errorf("open store: %w", err)
	}
	if _, err := db.Exec(schema); err != nil {
		db.Close()
		return nil, fmt.Errorf("init schema: %w", err)
	}
	if err := migrate(db, migrationsMonolith); err != nil {
		db.Close()
		return nil, err
	}
	return &Store{db: db}, nil
}

// Close closes the underlying database.
func (s *Store) Close() error { return s.db.Close() }

// Put durably persists a torrent's full identity (blob + metadata). It is an
// upsert keyed on info_hash: adding the same content twice can never collide.
func (s *Store) Put(r *Record) error {
	if r.InfoHash == "" {
		return fmt.Errorf("store: empty info_hash")
	}
	if len(r.Torrent) == 0 {
		return fmt.Errorf("store: empty torrent blob for %s", r.InfoHash)
	}
	s.wmux.Lock()
	defer s.wmux.Unlock()
	_, err := s.db.Exec(`
        INSERT INTO torrents
            (info_hash, session, torrent, save_path, category,
             added_time, completed_time, total_uploaded, total_downloaded,
             paused, tags)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(info_hash) DO UPDATE SET
            session          = excluded.session,
            torrent          = excluded.torrent,
            save_path        = excluded.save_path,
            category         = excluded.category,
            added_time       = excluded.added_time,
            completed_time   = excluded.completed_time,
            total_uploaded   = excluded.total_uploaded,
            total_downloaded = excluded.total_downloaded,
            paused           = excluded.paused,
            tags             = excluded.tags
    `, r.InfoHash, string(r.Session), r.Torrent, r.SavePath, r.Category,
		r.AddedTime, r.CompletedTime, r.TotalUploaded, r.TotalDownloaded,
		boolToInt(r.Paused), encodeTags(r.Tags))
	if err != nil {
		return fmt.Errorf("store put %s: %w", r.InfoHash, err)
	}
	return nil
}

// UpdateStats cheaply updates the lifetime counters and completion time without
// rewriting the (large) torrent blob — the hot path called from periodic saves.
func (s *Store) UpdateStats(infoHash string, completedTime float64, up, down int64) error {
	s.wmux.Lock()
	defer s.wmux.Unlock()
	_, err := s.db.Exec(`
        UPDATE torrents SET completed_time = ?, total_uploaded = ?, total_downloaded = ?
        WHERE info_hash = ?`, completedTime, up, down, infoHash)
	if err != nil {
		return fmt.Errorf("store update stats %s: %w", infoHash, err)
	}
	return nil
}

// UpdatePlacement updates category/save_path (a move) without touching the blob.
func (s *Store) UpdatePlacement(infoHash, category, savePath string) error {
	s.wmux.Lock()
	defer s.wmux.Unlock()
	_, err := s.db.Exec(`
        UPDATE torrents SET category = ?, save_path = ? WHERE info_hash = ?`,
		category, savePath, infoHash)
	if err != nil {
		return fmt.Errorf("store update placement %s: %w", infoHash, err)
	}
	return nil
}

// ClearCategory drops a category label from every torrent carrying it and
// reports how many rows changed. Deleting a category clears the label in the
// engines' memory; the store kept it, so the next boot reloaded those torrents
// with a category the user had deleted and it came back in the UI (issue #7).
// One statement rather than a row-per-torrent loop: at 100k+ torrents the
// difference is a single scan against as many transactions.
func (s *Store) ClearCategory(category string) (int64, error) {
	if category == "" {
		return 0, nil
	}
	s.wmux.Lock()
	defer s.wmux.Unlock()
	res, err := s.db.Exec(`UPDATE torrents SET category = '' WHERE category = ?`, category)
	if err != nil {
		return 0, fmt.Errorf("store clear category %q: %w", category, err)
	}
	n, _ := res.RowsAffected()
	return n, nil
}

// CategoryCount is a category label as the torrents carry it, with how many
// wear it. Distinct from the configured category list: a label outlives the
// category it names.
type CategoryCount struct {
	Name  string
	Count int
}

// CategoryCounts reports every category label present on a torrent. Used to
// surface labels that no longer match any configured category — the leftovers
// of deletions made before the label was cleared durably (issue #7), which the
// user otherwise has no way to reach.
func (s *Store) CategoryCounts() ([]CategoryCount, error) {
	rows, err := s.db.Query(
		`SELECT category, COUNT(*) FROM torrents WHERE category != '' GROUP BY category`)
	if err != nil {
		return nil, fmt.Errorf("store category counts: %w", err)
	}
	defer rows.Close()
	var out []CategoryCount
	for rows.Next() {
		var cc CategoryCount
		if err := rows.Scan(&cc.Name, &cc.Count); err != nil {
			return nil, err
		}
		out = append(out, cc)
	}
	return out, rows.Err()
}

// Delete removes a torrent's identity. Note: this never touches the payload
// files on disk — remove-torrent-keep-data is simply "don't delete the data".
func (s *Store) Delete(infoHash string) error {
	s.wmux.Lock()
	defer s.wmux.Unlock()
	_, err := s.db.Exec(`DELETE FROM torrents WHERE info_hash = ?`, infoHash)
	if err != nil {
		return fmt.Errorf("store delete %s: %w", infoHash, err)
	}
	return nil
}

// Get returns a single record. ok is false if no row exists.
func (s *Store) Get(infoHash string) (r *Record, ok bool, err error) {
	row := s.db.QueryRow(`
        SELECT info_hash, session, torrent, save_path, category,
               added_time, completed_time, total_uploaded, total_downloaded,
               paused, tags, content_folder
        FROM torrents WHERE info_hash = ?`, infoHash)
	rec, err := scan(row)
	if err == sql.ErrNoRows {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, err
	}
	return rec, true, nil
}

// BySession returns every record for a session (used to reload at boot).
func (s *Store) BySession(sess Session) ([]*Record, error) {
	rows, err := s.db.Query(`
        SELECT info_hash, session, torrent, save_path, category,
               added_time, completed_time, total_uploaded, total_downloaded,
               paused, tags, content_folder
        FROM torrents WHERE session = ?`, string(sess))
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanAll(rows)
}

// Count returns the number of stored torrents in a session.
func (s *Store) Count(sess Session) (int, error) {
	var n int
	err := s.db.QueryRow(`SELECT COUNT(*) FROM torrents WHERE session = ?`, string(sess)).Scan(&n)
	return n, err
}

type scanner interface{ Scan(dest ...any) error }

func scan(sc scanner) (*Record, error) {
	var r Record
	var sess string
	var paused int
	var tags string
	var contentFolder int
	if err := sc.Scan(&r.InfoHash, &sess, &r.Torrent, &r.SavePath, &r.Category,
		&r.AddedTime, &r.CompletedTime, &r.TotalUploaded, &r.TotalDownloaded,
		&paused, &tags, &contentFolder); err != nil {
		return nil, err
	}
	r.Session = Session(sess)
	r.Paused = paused != 0
	r.Tags = decodeTags(tags)
	// -1 means the row predates the flag: leave it nil so the engine applies
	// the legacy layout instead of an explicit "no folder".
	if contentFolder >= 0 {
		b := contentFolder != 0
		r.ContentFolder = &b
	}
	return &r, nil
}

func scanAll(rows *sql.Rows) ([]*Record, error) {
	var out []*Record
	for rows.Next() {
		r, err := scan(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, r)
	}
	return out, rows.Err()
}
