package store

import (
	"os"
	"path/filepath"
	"testing"
)

// The incident this guard exists for, in miniature.
//
// An install ran a version whose store would not open. Hydra carried on without
// it, and the code that keeps the carry-overs fell back to the JSON files,
// writing them from an in-memory state that had never been loaded from
// anywhere: a lifetime counter of almost nothing, and a category list holding
// only what the one screen the user happened to open put back. The next upgrade
// could open the store again, found those files sitting there, and imported
// them over the real numbers. The import is an overwrite, so the least
// trustworthy copy won and the originals were unrecoverable.
func TestReplayedSidecarsDoNotOverwriteTheStore(t *testing.T) {
	dir := t.TempDir()
	s, err := Open(filepath.Join(dir, "test.db"))
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer s.Close()

	write := func(name, body string) {
		t.Helper()
		if err := os.WriteFile(filepath.Join(dir, name), []byte(body), 0o644); err != nil {
			t.Fatal(err)
		}
	}

	// --- the genuine migration: real numbers move into the store ----------
	const realCats = `{"T4":{"save_path":"/downloads/","mode":"hoard"},"imported":{"save_path":"/downloads/","mode":"hoard"}}`
	write("baseline.json", `{"total_uploaded":2555924500630917,"total_downloaded":642314700933042}`)
	write("baseline_trackers.json", `[{"engine":"hoard","tracker":"t.example","ul":1073003152812,"dl":218385237454}]`)
	write("categories.json", realCats)

	if rep := MigrateSidecars(dir, s); len(rep.Errors) > 0 {
		t.Fatalf("first migration errors: %v", rep.Errors)
	}
	if ul, _, _ := s.CounterGet(CounterGlobal); ul != 2555924500630917 {
		t.Fatalf("first migration did not carry the counter over: ul=%d", ul)
	}

	// --- the degraded boot writes the files again, from nothing -----------
	write("baseline.json", `{"total_uploaded":0,"total_downloaded":0}`)
	write("baseline_trackers.json", `[]`)
	write("categories.json", `{"T4":{"save_path":"/downloads/","mode":"hoard"}}`)

	// --- the store opens again, and must refuse all three ------------------
	rep := MigrateSidecars(dir, s)
	if len(rep.Errors) > 0 {
		t.Fatalf("replay errors: %v", rep.Errors)
	}
	if len(rep.Superseded) != 3 {
		t.Fatalf("superseded = %v, want all three named", rep.Superseded)
	}

	ul, dl, _ := s.CounterGet(CounterGlobal)
	if ul != 2555924500630917 || dl != 642314700933042 {
		t.Fatalf("the lifetime counter was overwritten by the replay: ul=%d dl=%d", ul, dl)
	}
	if got, _ := s.GetMeta(MetaCategories); got != realCats {
		t.Fatalf("the category list was overwritten by the replay:\n got %s\nwant %s", got, realCats)
	}
	if tul, _, _ := s.CounterGet(TrackerCounterKey("hoard", "t.example")); tul != 1073003152812 {
		t.Fatalf("the tracker carry-over was overwritten: ul=%d", tul)
	}

	// The refused files are kept, under a name that says why: they are still
	// the only record of what the degraded run believed.
	for _, n := range []string{"baseline.json", "baseline_trackers.json", "categories.json"} {
		if _, err := os.Stat(filepath.Join(dir, n+".superseded")); err != nil {
			t.Errorf("%s should have been set aside as .superseded", n)
		}
		if _, err := os.Stat(filepath.Join(dir, n)); !os.IsNotExist(err) {
			t.Errorf("%s should no longer be in the way", n)
		}
	}
}

// Installs that migrated before the marker existed have no marker to check, so
// the value itself has to be the guard: a store that already holds a lifetime
// counter must not take one from a file, first import here or not.
func TestFirstImportStillProtectsAStoreThatAlreadyHasTheValue(t *testing.T) {
	dir := t.TempDir()
	s, err := Open(filepath.Join(dir, "test.db"))
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer s.Close()

	if err := s.CounterSet(CounterGlobal, 999_000_000, 111_000_000); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "baseline.json"),
		[]byte(`{"total_uploaded":1,"total_downloaded":1}`), 0o644); err != nil {
		t.Fatal(err)
	}

	rep := MigrateSidecars(dir, s)
	if len(rep.Errors) > 0 {
		t.Fatalf("errors: %v", rep.Errors)
	}
	if ul, dl, _ := s.CounterGet(CounterGlobal); ul != 999_000_000 || dl != 111_000_000 {
		t.Fatalf("a populated counter was replaced from a file: ul=%d dl=%d", ul, dl)
	}
	if len(rep.Superseded) != 1 {
		t.Errorf("superseded = %v, want baseline.json", rep.Superseded)
	}
}

// The ordinary path must keep working: a fresh store takes everything the
// sidecars hold, which is the whole point of the migration.
func TestFirstMigrationStillImports(t *testing.T) {
	dir := t.TempDir()
	s, err := Open(filepath.Join(dir, "test.db"))
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer s.Close()

	if err := os.WriteFile(filepath.Join(dir, "baseline.json"),
		[]byte(`{"total_uploaded":42,"total_downloaded":7}`), 0o644); err != nil {
		t.Fatal(err)
	}

	rep := MigrateSidecars(dir, s)
	if len(rep.Errors) > 0 || len(rep.Superseded) > 0 {
		t.Fatalf("a plain migration was refused: errors=%v superseded=%v", rep.Errors, rep.Superseded)
	}
	if ul, dl, _ := s.CounterGet(CounterGlobal); ul != 42 || dl != 7 {
		t.Fatalf("ul=%d dl=%d, want 42/7", ul, dl)
	}
	if _, err := os.Stat(filepath.Join(dir, "baseline.json.migrated")); err != nil {
		t.Error("the original should have been retired, not superseded")
	}
}
