package store

import (
	"database/sql"
	"fmt"
	"strconv"
)

// MetaCategoryBackfill records that the one-shot category repair has run.
//
// v3.50.0 retired state.json but handed the store only the content-layout flag,
// so every torrent added before the move lost its category: the column had
// never been filled for them, and the boot-time overlay that used to hide the
// gap was removed in the same commit. The categories still exist in the
// state.json.migrated the upgrade left behind, so the repair is a backfill.
//
// It must run exactly once. An empty category is a legitimate choice, so a
// repair keyed on emptiness alone would undo a user's deliberate clearing on
// every boot that followed.
//
// v2, not v1: the v3.52.1 repair wrote the categories back and then watched
// them vanish within the same boot. The engine had already rebuilt its torrents
// from resume data, which carries no category, and the store import dropped the
// store's record for every torrent it found there -- so the next sync wrote
// that blank view straight back over the repair. v1's marker was left behind
// claiming a repair that no longer exists on disk, so the fixed build needs a
// key of its own to be allowed to run again. Anyone who cleared a category by
// hand between the two gets it back once; there was no boot in between where
// the store held their choice anyway.
const MetaCategoryBackfill = "category_backfill_v2"

// BackfillCategories restores the categories the v3.50.0 store migration
// dropped, then records that it has done so. It reports ran=false and touches
// nothing if the marker is already set.
//
// The updates and the marker share a transaction, so an interrupted boot either
// repaired every row and recorded it, or did neither.
func (s *Store) BackfillCategories(cats map[string]string) (updated int, ran bool, err error) {
	s.wmux.Lock()
	defer s.wmux.Unlock()

	tx, err := s.db.Begin()
	if err != nil {
		return 0, false, fmt.Errorf("category backfill: begin: %w", err)
	}
	defer tx.Rollback()

	var marker string
	switch scanErr := tx.QueryRow(
		`SELECT value FROM meta WHERE key = ?`, MetaCategoryBackfill).Scan(&marker); {
	case scanErr == sql.ErrNoRows:
		// Never run here: go ahead.
	case scanErr != nil:
		return 0, false, fmt.Errorf("category backfill: read marker: %w", scanErr)
	default:
		return 0, false, nil
	}

	// Two guards, for two different things. `category = ''` protects the
	// torrents a user has already placed by hand since the upgrade, within this
	// run. The marker protects every boot after it.
	stmt, err := tx.Prepare(
		`UPDATE torrents SET category = ? WHERE info_hash = ? AND category = ''`)
	if err != nil {
		return 0, false, fmt.Errorf("category backfill: prepare: %w", err)
	}
	defer stmt.Close()

	for infoHash, cat := range cats {
		if cat == "" {
			continue
		}
		res, execErr := stmt.Exec(cat, infoHash)
		if execErr != nil {
			return 0, false, fmt.Errorf("category backfill: %s: %w", infoHash, execErr)
		}
		n, _ := res.RowsAffected()
		updated += int(n)
	}

	if _, err := tx.Exec(`INSERT INTO meta (key, value) VALUES (?, ?)`,
		MetaCategoryBackfill, strconv.Itoa(updated)); err != nil {
		return 0, false, fmt.Errorf("category backfill: set marker: %w", err)
	}
	if err := tx.Commit(); err != nil {
		return 0, false, fmt.Errorf("category backfill: commit: %w", err)
	}
	return updated, true, nil
}
