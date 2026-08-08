package store

import (
	"os"
	"path/filepath"
	"testing"
)

// The category document is moved byte-for-byte: whatever fields it carries
// today or grows tomorrow, the migration must not reinterpret them.
func TestMigrateSidecarDocuments(t *testing.T) {
	dir := t.TempDir()
	s, err := Open(filepath.Join(dir, "test.db"))
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer s.Close()

	cats := `{"movies":{"save_path":"/data/movies","mode":"hoard","graduate_to":"archive","placement":["a","b"]}}`
	prov := `{"source_client":"Transmission","source_date":42,"carried_uploaded_bytes":7,"imported_count":3}`
	os.WriteFile(filepath.Join(dir, "categories.json"), []byte(cats), 0o644)
	os.WriteFile(filepath.Join(dir, "provenance.json"), []byte(prov), 0o644)

	rep := MigrateSidecars(dir, s)
	if len(rep.Errors) != 0 {
		t.Fatalf("errors: %v", rep.Errors)
	}
	if rep.Documents != 2 {
		t.Errorf("documents = %d, want 2", rep.Documents)
	}
	got, _ := s.GetMeta(MetaCategories)
	if got != cats {
		t.Errorf("categories changed on the way in:\n got %s\nwant %s", got, cats)
	}
	if got, _ := s.GetMeta(MetaProvenance); got != prov {
		t.Errorf("provenance = %s", got)
	}
	// The originals are kept, renamed: a rollback is a copy back.
	if _, err := os.Stat(filepath.Join(dir, "categories.json")); !os.IsNotExist(err) {
		t.Error("categories.json should have been retired")
	}
	if _, err := os.Stat(filepath.Join(dir, "categories.json.migrated")); err != nil {
		t.Error("the retired copy should still be there")
	}

	// A second boot finds nothing to do and must not wipe what was imported.
	rep2 := MigrateSidecars(dir, s)
	if rep2.Documents != 0 {
		t.Errorf("second run migrated %d documents, want 0", rep2.Documents)
	}
	if got, _ := s.GetMeta(MetaCategories); got != cats {
		t.Error("second run clobbered the categories")
	}
}

// An empty or missing file is the normal case and must be a no-op, not an error.
func TestMigrateSidecarDocumentsAbsent(t *testing.T) {
	dir := t.TempDir()
	s, err := Open(filepath.Join(dir, "test.db"))
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer s.Close()
	os.WriteFile(filepath.Join(dir, "provenance.json"), nil, 0o644)

	rep := MigrateSidecars(dir, s)
	if rep.Documents != 0 || len(rep.Errors) != 0 {
		t.Errorf("docs=%d errors=%v", rep.Documents, rep.Errors)
	}
}
