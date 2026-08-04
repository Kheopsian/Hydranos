package store

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"sync"
)

// Per-torrent user state: the pause intent and the tag list.
//
// Both used to live outside the database — pause did not persist at all, tags
// sat in a tags.json overlay. They are here now because losing either has a
// consequence the user cannot undo by hand: a lost pause silently resumes
// seeding (ratio, tracker rules), and a lost tag set is hours of curation. The
// deciding property is that they are written in the same transaction as the
// torrent row, so a removed torrent can never leave either behind.

func boolToInt(b bool) int {
	if b {
		return 1
	}
	return 0
}

// encodeTags stores the tag list as a JSON array. Empty stays the empty string
// rather than "[]" so the column reads naturally and the default is honest.
func encodeTags(tags []string) string {
	if len(tags) == 0 {
		return ""
	}
	b, err := json.Marshal(tags)
	if err != nil {
		return ""
	}
	return string(b)
}

func decodeTags(s string) []string {
	if s == "" {
		return nil
	}
	var out []string
	if json.Unmarshal([]byte(s), &out) != nil {
		return nil
	}
	return out
}

func setPaused(db *sql.DB, mux *sync.Mutex, infoHash string, paused bool) error {
	mux.Lock()
	defer mux.Unlock()
	_, err := db.Exec(`UPDATE torrents SET paused = ? WHERE info_hash = ?`, boolToInt(paused), infoHash)
	if err != nil {
		return fmt.Errorf("store set paused %s: %w", infoHash, err)
	}
	return nil
}

// setPausedMany applies one intent to a batch in a single transaction, so a
// multi-select in the UI is one durable decision rather than N of them.
func setPausedMany(db *sql.DB, mux *sync.Mutex, infoHashes []string, paused bool) (int, error) {
	if len(infoHashes) == 0 {
		return 0, nil
	}
	mux.Lock()
	defer mux.Unlock()
	tx, err := db.Begin()
	if err != nil {
		return 0, err
	}
	defer tx.Rollback()
	stmt, err := tx.Prepare(`UPDATE torrents SET paused = ? WHERE info_hash = ?`)
	if err != nil {
		return 0, err
	}
	defer stmt.Close()
	n := 0
	for _, ih := range infoHashes {
		res, err := stmt.Exec(boolToInt(paused), ih)
		if err != nil {
			return 0, fmt.Errorf("store set paused %s: %w", ih, err)
		}
		if aff, _ := res.RowsAffected(); aff > 0 {
			n++
		}
	}
	if err := tx.Commit(); err != nil {
		return 0, err
	}
	return n, nil
}

// setPausedAll is the whole-session switch behind pause-all / resume-all. It is
// one statement whatever the torrent count, which is why the intent can be
// persisted for every torrent without a 100k-row write storm.
func setPausedAll(db *sql.DB, mux *sync.Mutex, where string, args []any, paused bool) (int, error) {
	mux.Lock()
	defer mux.Unlock()
	q := `UPDATE torrents SET paused = ?`
	if where != "" {
		q += ` WHERE ` + where
	}
	res, err := db.Exec(q, append([]any{boolToInt(paused)}, args...)...)
	if err != nil {
		return 0, fmt.Errorf("store set paused all: %w", err)
	}
	n, _ := res.RowsAffected()
	return int(n), nil
}

func setTags(db *sql.DB, mux *sync.Mutex, infoHash string, tags []string) error {
	mux.Lock()
	defer mux.Unlock()
	_, err := db.Exec(`UPDATE torrents SET tags = ? WHERE info_hash = ?`, encodeTags(tags), infoHash)
	if err != nil {
		return fmt.Errorf("store set tags %s: %w", infoHash, err)
	}
	return nil
}

func allTags(db *sql.DB) (map[string][]string, error) {
	rows, err := db.Query(`SELECT info_hash, tags FROM torrents WHERE tags != ''`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := map[string][]string{}
	for rows.Next() {
		var ih, t string
		if err := rows.Scan(&ih, &t); err != nil {
			return nil, err
		}
		if tags := decodeTags(t); len(tags) > 0 {
			out[ih] = tags
		}
	}
	return out, rows.Err()
}

func pausedSet(db *sql.DB) (map[string]bool, error) {
	rows, err := db.Query(`SELECT info_hash FROM torrents WHERE paused != 0`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := map[string]bool{}
	for rows.Next() {
		var ih string
		if err := rows.Scan(&ih); err != nil {
			return nil, err
		}
		out[ih] = true
	}
	return out, rows.Err()
}

// --- tag registry -----------------------------------------------------------
//
// qBittorrent parity: a tag can exist before anything wears it, so the set of
// known names cannot be derived from the torrents alone.

func addTagNames(db *sql.DB, mux *sync.Mutex, names []string) error {
	if len(names) == 0 {
		return nil
	}
	mux.Lock()
	defer mux.Unlock()
	tx, err := db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	stmt, err := tx.Prepare(`INSERT INTO tag_registry (name) VALUES (?) ON CONFLICT(name) DO NOTHING`)
	if err != nil {
		return err
	}
	defer stmt.Close()
	for _, n := range names {
		if n == "" {
			continue
		}
		if _, err := stmt.Exec(n); err != nil {
			return fmt.Errorf("store add tag %q: %w", n, err)
		}
	}
	return tx.Commit()
}

func deleteTagNames(db *sql.DB, mux *sync.Mutex, names []string) error {
	if len(names) == 0 {
		return nil
	}
	mux.Lock()
	defer mux.Unlock()
	tx, err := db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	stmt, err := tx.Prepare(`DELETE FROM tag_registry WHERE name = ?`)
	if err != nil {
		return err
	}
	defer stmt.Close()
	for _, n := range names {
		if _, err := stmt.Exec(n); err != nil {
			return fmt.Errorf("store delete tag %q: %w", n, err)
		}
	}
	return tx.Commit()
}

func tagNames(db *sql.DB) ([]string, error) {
	rows, err := db.Query(`SELECT name FROM tag_registry ORDER BY name`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []string
	for rows.Next() {
		var n string
		if err := rows.Scan(&n); err != nil {
			return nil, err
		}
		out = append(out, n)
	}
	return out, rows.Err()
}

// --- public methods, monolith store ----------------------------------------

// SetPaused records (or clears) the user's pause intent for one torrent.
func (s *Store) SetPaused(infoHash string, paused bool) error {
	return setPaused(s.db, &s.wmux, infoHash, paused)
}

// SetPausedMany applies one intent to a batch in a single transaction.
func (s *Store) SetPausedMany(infoHashes []string, paused bool) (int, error) {
	return setPausedMany(s.db, &s.wmux, infoHashes, paused)
}

// SetPausedSession applies one intent to every torrent of a session.
func (s *Store) SetPausedSession(sess Session, paused bool) (int, error) {
	return setPausedAll(s.db, &s.wmux, `session = ?`, []any{string(sess)}, paused)
}

// PausedSet returns the info hashes the user has paused, for boot.
func (s *Store) PausedSet() (map[string]bool, error) { return pausedSet(s.db) }

// SetTags replaces a torrent's tag list.
func (s *Store) SetTags(infoHash string, tags []string) error {
	return setTags(s.db, &s.wmux, infoHash, tags)
}

// AllTags returns info_hash -> tags for every tagged torrent, for boot.
func (s *Store) AllTags() (map[string][]string, error) { return allTags(s.db) }

// AddTagNames registers tag names (including ones not yet assigned).
func (s *Store) AddTagNames(names []string) error { return addTagNames(s.db, &s.wmux, names) }

// DeleteTagNames unregisters tag names.
func (s *Store) DeleteTagNames(names []string) error { return deleteTagNames(s.db, &s.wmux, names) }

// TagNames returns every known tag name.
func (s *Store) TagNames() ([]string, error) { return tagNames(s.db) }

// --- public methods, per-agent store ---------------------------------------

// SetPaused records (or clears) the user's pause intent for one torrent.
func (s *AgentStore) SetPaused(infoHash string, paused bool) error {
	return setPaused(s.db, &s.wmux, infoHash, paused)
}

// SetPausedMany applies one intent to a batch in a single transaction.
func (s *AgentStore) SetPausedMany(infoHashes []string, paused bool) (int, error) {
	return setPausedMany(s.db, &s.wmux, infoHashes, paused)
}

// SetPausedAll applies one intent to every torrent this agent owns.
func (s *AgentStore) SetPausedAll(paused bool) (int, error) {
	return setPausedAll(s.db, &s.wmux, "", nil, paused)
}

// PausedSet returns the info hashes the user has paused, for boot.
func (s *AgentStore) PausedSet() (map[string]bool, error) { return pausedSet(s.db) }

// SetTags replaces a torrent's tag list.
func (s *AgentStore) SetTags(infoHash string, tags []string) error {
	return setTags(s.db, &s.wmux, infoHash, tags)
}

// AllTags returns info_hash -> tags for every tagged torrent, for boot.
func (s *AgentStore) AllTags() (map[string][]string, error) { return allTags(s.db) }

// AddTagNames registers tag names (including ones not yet assigned).
func (s *AgentStore) AddTagNames(names []string) error { return addTagNames(s.db, &s.wmux, names) }

// DeleteTagNames unregisters tag names.
func (s *AgentStore) DeleteTagNames(names []string) error {
	return deleteTagNames(s.db, &s.wmux, names)
}

// TagNames returns every known tag name.
func (s *AgentStore) TagNames() ([]string, error) { return tagNames(s.db) }
