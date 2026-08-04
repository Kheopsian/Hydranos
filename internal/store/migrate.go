package store

import (
	"database/sql"
	"fmt"
)

// Schema evolution.
//
// The base `schema` / `schemaAgent` constants are the v0 baseline and must not
// change again: `CREATE TABLE IF NOT EXISTS` is a no-op against a database that
// already has the table, so a column added there would exist only for users who
// started fresh. Every later change is an entry in the lists below, applied in
// order and recorded in `PRAGMA user_version`, so a 2.4 GB database created
// before the change and an empty one created after it converge on the same
// shape.
//
// Adding a column is metadata-only in SQLite — instant even on a large file.

// migrationsMonolith evolves the session-keyed store (store.go).
var migrationsMonolith = []string{
	// v1: durable per-torrent user state + the counters and tag registry that
	// used to live in JSON sidecars next to the database.
	`
	ALTER TABLE torrents ADD COLUMN paused INTEGER NOT NULL DEFAULT 0;
	ALTER TABLE torrents ADD COLUMN tags TEXT NOT NULL DEFAULT '';
	CREATE TABLE IF NOT EXISTS counters (
	    key TEXT PRIMARY KEY,
	    ul  INTEGER NOT NULL DEFAULT 0,
	    dl  INTEGER NOT NULL DEFAULT 0
	);
	CREATE TABLE IF NOT EXISTS tag_registry (
	    name TEXT PRIMARY KEY
	);
	`,
}

// migrationsAgent evolves the per-agent store (store_agent.go). Kept as its own
// list because the two schemas differ (no `session` column here); the version
// counters are independent, one per database file.
var migrationsAgent = []string{
	// v1: mirror of the monolith's v1.
	`
	ALTER TABLE torrents ADD COLUMN paused INTEGER NOT NULL DEFAULT 0;
	ALTER TABLE torrents ADD COLUMN tags TEXT NOT NULL DEFAULT '';
	CREATE TABLE IF NOT EXISTS counters (
	    key TEXT PRIMARY KEY,
	    ul  INTEGER NOT NULL DEFAULT 0,
	    dl  INTEGER NOT NULL DEFAULT 0
	);
	CREATE TABLE IF NOT EXISTS tag_registry (
	    name TEXT PRIMARY KEY
	);
	`,
}

// migrate applies every migration the database has not seen yet, each in its own
// transaction together with the version bump, so an interrupted upgrade either
// fully applied a step or did not apply it at all.
func migrate(db *sql.DB, migrations []string) error {
	var version int
	if err := db.QueryRow(`PRAGMA user_version`).Scan(&version); err != nil {
		return fmt.Errorf("read schema version: %w", err)
	}
	if version > len(migrations) {
		// The file was written by a newer Hydra. Refuse rather than run against
		// a shape we do not know: downgrading should be a deliberate act.
		return fmt.Errorf("database schema is version %d but this build only knows %d — "+
			"it was written by a newer Hydra; upgrade back or restore a backup",
			version, len(migrations))
	}
	for i := version; i < len(migrations); i++ {
		tx, err := db.Begin()
		if err != nil {
			return fmt.Errorf("migration %d: begin: %w", i+1, err)
		}
		if _, err := tx.Exec(migrations[i]); err != nil {
			tx.Rollback()
			return fmt.Errorf("migration %d: %w", i+1, err)
		}
		// PRAGMA does not accept bound parameters; i+1 is an int we control.
		if _, err := tx.Exec(fmt.Sprintf(`PRAGMA user_version = %d`, i+1)); err != nil {
			tx.Rollback()
			return fmt.Errorf("migration %d: set version: %w", i+1, err)
		}
		if err := tx.Commit(); err != nil {
			return fmt.Errorf("migration %d: commit: %w", i+1, err)
		}
	}
	return nil
}
