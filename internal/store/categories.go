package store

import "database/sql"

// Small named documents that used to be JSON files sitting next to the
// database: the category list, the import provenance. They are read and
// written as a whole and their shape follows the features, so they are stored
// as documents rather than as a schema that would need a migration per field.

// MetaCategories and MetaProvenance are the keys in use.
const (
	MetaCategories = "categories"
	MetaProvenance = "provenance"
)

// GetMeta reads a document. Missing is not an error: it returns "".
func (s *Store) GetMeta(key string) (string, error) {
	var v string
	err := s.db.QueryRow(`SELECT value FROM meta WHERE key = ?`, key).Scan(&v)
	if err == sql.ErrNoRows {
		return "", nil
	}
	return v, err
}

// SetMeta writes a document.
func (s *Store) SetMeta(key, value string) error {
	s.wmux.Lock()
	defer s.wmux.Unlock()
	_, err := s.db.Exec(
		`INSERT INTO meta (key, value) VALUES (?, ?)
		 ON CONFLICT(key) DO UPDATE SET value = excluded.value`, key, value)
	return err
}
