package store

import (
	"database/sql"
	"fmt"
	"strings"
	"sync"
)

// Lifetime byte counters — the carry-over totals that keep global and
// per-tracker upload monotonic when a torrent is removed.
//
// These lived in baseline.json / baseline_trackers.json. Both wrote atomically,
// so corruption was never the problem; the problem was that absorbing a
// torrent's lifetime bytes and deleting its row were two writes to two files
// with no transaction spanning them. A crash in between either double-counted
// the torrent on the next boot or dropped its bytes for good — and this is the
// one number in Hydra that cannot be recomputed from anything else. Holding the
// counters here lets the removal path be a single COMMIT, which turns
// monotonicity from a discipline into a property of the schema.

// CounterGlobal is the key for the all-engines lifetime carry-over.
const CounterGlobal = "global"

// TrackerCounterKey is the key for one (engine, tracker) carry-over row. The
// NUL separator matches the in-memory key the API layer already uses, so the
// two cannot disagree on what counts as the same tracker.
func TrackerCounterKey(engine, tracker string) string {
	return "tracker\x00" + engine + "\x00" + tracker
}

// ParseTrackerCounterKey splits a tracker key back into its parts; ok is false
// for any other kind of key.
func ParseTrackerCounterKey(key string) (engine, tracker string, ok bool) {
	rest, found := strings.CutPrefix(key, "tracker\x00")
	if !found {
		return "", "", false
	}
	engine, tracker, found = strings.Cut(rest, "\x00")
	return engine, tracker, found
}

func counterAdd(tx *sql.Tx, key string, ul, dl int64) error {
	_, err := tx.Exec(`
        INSERT INTO counters (key, ul, dl) VALUES (?, ?, ?)
        ON CONFLICT(key) DO UPDATE SET ul = ul + excluded.ul, dl = dl + excluded.dl`,
		key, ul, dl)
	if err != nil {
		return fmt.Errorf("counter add %q: %w", key, err)
	}
	return nil
}

func counterAddMany(db *sql.DB, mux *sync.Mutex, keys []string, ul, dl int64) error {
	if len(keys) == 0 || (ul == 0 && dl == 0) {
		return nil
	}
	mux.Lock()
	defer mux.Unlock()
	tx, err := db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	for _, k := range keys {
		if err := counterAdd(tx, k, ul, dl); err != nil {
			return err
		}
	}
	return tx.Commit()
}

func counterSet(db *sql.DB, mux *sync.Mutex, key string, ul, dl int64) error {
	mux.Lock()
	defer mux.Unlock()
	_, err := db.Exec(`
        INSERT INTO counters (key, ul, dl) VALUES (?, ?, ?)
        ON CONFLICT(key) DO UPDATE SET ul = excluded.ul, dl = excluded.dl`, key, ul, dl)
	if err != nil {
		return fmt.Errorf("counter set %q: %w", key, err)
	}
	return nil
}

func counterGet(db *sql.DB, key string) (ul, dl int64, err error) {
	err = db.QueryRow(`SELECT ul, dl FROM counters WHERE key = ?`, key).Scan(&ul, &dl)
	if err == sql.ErrNoRows {
		return 0, 0, nil
	}
	return ul, dl, err
}

func countersAll(db *sql.DB) (map[string][2]int64, error) {
	rows, err := db.Query(`SELECT key, ul, dl FROM counters`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	out := map[string][2]int64{}
	for rows.Next() {
		var k string
		var ul, dl int64
		if err := rows.Scan(&k, &ul, &dl); err != nil {
			return nil, err
		}
		out[k] = [2]int64{ul, dl}
	}
	return out, rows.Err()
}

// deleteAbsorb removes a torrent and folds its lifetime bytes into the given
// counter keys in one transaction — the whole point of moving the counters into
// the database. Either the torrent is gone and its bytes are carried, or
// neither happened.
func deleteAbsorb(db *sql.DB, mux *sync.Mutex, infoHash string, keys []string, ul, dl int64) error {
	mux.Lock()
	defer mux.Unlock()
	tx, err := db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if _, err := tx.Exec(`DELETE FROM torrents WHERE info_hash = ?`, infoHash); err != nil {
		return fmt.Errorf("delete-absorb %s: %w", infoHash, err)
	}
	if ul > 0 || dl > 0 {
		for _, k := range keys {
			if k == "" {
				continue
			}
			if err := counterAdd(tx, k, ul, dl); err != nil {
				return err
			}
		}
	}
	return tx.Commit()
}

// --- public methods, monolith store ----------------------------------------

// CounterAdd folds bytes into one or more carry-over counters.
func (s *Store) CounterAdd(keys []string, ul, dl int64) error {
	return counterAddMany(s.db, &s.wmux, keys, ul, dl)
}

// CounterSet overwrites a counter (used by the one-shot sidecar import).
func (s *Store) CounterSet(key string, ul, dl int64) error {
	return counterSet(s.db, &s.wmux, key, ul, dl)
}

// CounterGet reads one counter; a missing key reads as zero.
func (s *Store) CounterGet(key string) (ul, dl int64, err error) { return counterGet(s.db, key) }

// CountersAll returns every counter, for boot.
func (s *Store) CountersAll() (map[string][2]int64, error) { return countersAll(s.db) }

// DeleteAbsorb removes a torrent and carries its lifetime bytes into the given
// counters atomically.
func (s *Store) DeleteAbsorb(infoHash string, keys []string, ul, dl int64) error {
	return deleteAbsorb(s.db, &s.wmux, infoHash, keys, ul, dl)
}

// --- public methods, per-agent store ---------------------------------------

// CounterAdd folds bytes into one or more carry-over counters.
func (s *AgentStore) CounterAdd(keys []string, ul, dl int64) error {
	return counterAddMany(s.db, &s.wmux, keys, ul, dl)
}

// CounterSet overwrites a counter.
func (s *AgentStore) CounterSet(key string, ul, dl int64) error {
	return counterSet(s.db, &s.wmux, key, ul, dl)
}

// CounterGet reads one counter; a missing key reads as zero.
func (s *AgentStore) CounterGet(key string) (ul, dl int64, err error) { return counterGet(s.db, key) }

// CountersAll returns every counter, for boot.
func (s *AgentStore) CountersAll() (map[string][2]int64, error) { return countersAll(s.db) }

// DeleteAbsorb removes a torrent and carries its lifetime bytes into the given
// counters atomically.
func (s *AgentStore) DeleteAbsorb(infoHash string, keys []string, ul, dl int64) error {
	return deleteAbsorb(s.db, &s.wmux, infoHash, keys, ul, dl)
}
