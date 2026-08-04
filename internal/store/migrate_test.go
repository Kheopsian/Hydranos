package store

import (
	"database/sql"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

// legacyDB writes a database in the exact shape Hydra shipped before the
// schema-version machinery existed: base tables only, user_version still 0.
// Every test that matters here starts from one of these, because the failure
// mode being guarded against is "works on a fresh database, breaks on 2.4 GB of
// someone's real data".
func legacyDB(t *testing.T, path string, rows ...*Record) {
	t.Helper()
	db, err := sql.Open("sqlite", "file:"+path)
	if err != nil {
		t.Fatalf("open legacy db: %v", err)
	}
	defer db.Close()
	if _, err := db.Exec(schema); err != nil {
		t.Fatalf("legacy schema: %v", err)
	}
	for _, r := range rows {
		_, err := db.Exec(`
            INSERT INTO torrents (info_hash, session, torrent, save_path, category,
                                  added_time, completed_time, total_uploaded, total_downloaded)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
			r.InfoHash, string(r.Session), r.Torrent, r.SavePath, r.Category,
			r.AddedTime, r.CompletedTime, r.TotalUploaded, r.TotalDownloaded)
		if err != nil {
			t.Fatalf("legacy insert: %v", err)
		}
	}
	var v int
	if err := db.QueryRow(`PRAGMA user_version`).Scan(&v); err != nil {
		t.Fatalf("read version: %v", err)
	}
	if v != 0 {
		t.Fatalf("legacy db should be at version 0, got %d", v)
	}
}

func TestMigrateUpgradesExistingDatabase(t *testing.T) {
	path := filepath.Join(t.TempDir(), "hydra.db")
	legacyDB(t, path, &Record{
		InfoHash: "aaaa", Session: Hoard, Torrent: []byte("blob"),
		SavePath: "/data/x", Category: "movies", TotalUploaded: 4242,
	})

	s, err := Open(path)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer s.Close()

	// The pre-existing row survives, and reads back through the new columns.
	rec, ok, err := s.Get("aaaa")
	if err != nil || !ok {
		t.Fatalf("get after migrate: ok=%v err=%v", ok, err)
	}
	if rec.SavePath != "/data/x" || rec.Category != "movies" || rec.TotalUploaded != 4242 {
		t.Fatalf("legacy data mangled by migration: %+v", rec)
	}
	if rec.Paused {
		t.Fatalf("a torrent that predates the pause column must not read as paused")
	}
	if len(rec.Tags) != 0 {
		t.Fatalf("unexpected tags: %v", rec.Tags)
	}

	// The new tables exist and are usable.
	if err := s.CounterSet(CounterGlobal, 1, 2); err != nil {
		t.Fatalf("counters unusable after migrate: %v", err)
	}
	if err := s.AddTagNames([]string{"hi"}); err != nil {
		t.Fatalf("tag registry unusable after migrate: %v", err)
	}
}

func TestMigrateIsIdempotent(t *testing.T) {
	path := filepath.Join(t.TempDir(), "hydra.db")
	legacyDB(t, path, &Record{InfoHash: "bbbb", Session: Race, Torrent: []byte("b")})

	for i := 0; i < 3; i++ {
		s, err := Open(path)
		if err != nil {
			t.Fatalf("open #%d: %v", i, err)
		}
		if err := s.SetPaused("bbbb", true); err != nil {
			t.Fatalf("set paused #%d: %v", i, err)
		}
		s.Close()
	}

	s, err := Open(path)
	if err != nil {
		t.Fatalf("final open: %v", err)
	}
	defer s.Close()
	rec, ok, _ := s.Get("bbbb")
	if !ok || !rec.Paused {
		t.Fatalf("pause intent lost across reopens: ok=%v rec=%+v", ok, rec)
	}
}

func TestMigrateRefusesNewerSchema(t *testing.T) {
	path := filepath.Join(t.TempDir(), "hydra.db")
	legacyDB(t, path)
	db, err := sql.Open("sqlite", "file:"+path)
	if err != nil {
		t.Fatal(err)
	}
	// Pretend a future Hydra wrote this file.
	if _, err := db.Exec(`PRAGMA user_version = 99`); err != nil {
		t.Fatal(err)
	}
	db.Close()

	if _, err := Open(path); err == nil {
		t.Fatal("opening a database from a newer Hydra must fail loudly, not silently run against an unknown shape")
	}
}

func TestAgentStoreMigrates(t *testing.T) {
	path := filepath.Join(t.TempDir(), "race.db")
	db, err := sql.Open("sqlite", "file:"+path)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec(schemaAgent); err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec(`INSERT INTO torrents (info_hash, torrent) VALUES ('cccc', 'x')`); err != nil {
		t.Fatal(err)
	}
	db.Close()

	s, err := OpenAgent(path)
	if err != nil {
		t.Fatalf("open agent: %v", err)
	}
	defer s.Close()
	if err := s.SetPaused("cccc", true); err != nil {
		t.Fatalf("set paused: %v", err)
	}
	rec, ok, err := s.Get("cccc")
	if err != nil || !ok || !rec.Paused {
		t.Fatalf("agent pause round-trip failed: ok=%v err=%v rec=%+v", ok, err, rec)
	}
}

func TestPauseIntentRoundTrip(t *testing.T) {
	s, err := Open(filepath.Join(t.TempDir(), "hydra.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	for _, ih := range []string{"a", "b", "c"} {
		if err := s.Put(&Record{InfoHash: ih, Session: Hoard, Torrent: []byte("x")}); err != nil {
			t.Fatal(err)
		}
	}

	if n, err := s.SetPausedMany([]string{"a", "c"}, true); err != nil || n != 2 {
		t.Fatalf("SetPausedMany = %d, %v", n, err)
	}
	paused, err := s.PausedSet()
	if err != nil {
		t.Fatal(err)
	}
	if !paused["a"] || !paused["c"] || paused["b"] {
		t.Fatalf("wrong paused set: %v", paused)
	}

	// A blob-rewriting Put must not silently clear the intent it carries.
	rec, _, _ := s.Get("a")
	if err := s.Put(rec); err != nil {
		t.Fatal(err)
	}
	if rec, _, _ := s.Get("a"); !rec.Paused {
		t.Fatal("Put dropped the pause intent")
	}

	if n, err := s.SetPausedSession(Hoard, false); err != nil || n != 3 {
		t.Fatalf("SetPausedSession = %d, %v", n, err)
	}
	paused, _ = s.PausedSet()
	if len(paused) != 0 {
		t.Fatalf("resume-all left intents behind: %v", paused)
	}
}

func TestTagsRoundTrip(t *testing.T) {
	s, err := Open(filepath.Join(t.TempDir(), "hydra.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	if err := s.Put(&Record{InfoHash: "a", Session: Hoard, Torrent: []byte("x"),
		Tags: []string{"fr", "1080p"}}); err != nil {
		t.Fatal(err)
	}
	rec, _, _ := s.Get("a")
	if len(rec.Tags) != 2 || rec.Tags[0] != "fr" || rec.Tags[1] != "1080p" {
		t.Fatalf("tags round-trip: %v", rec.Tags)
	}
	if err := s.SetTags("a", nil); err != nil {
		t.Fatal(err)
	}
	if rec, _, _ := s.Get("a"); len(rec.Tags) != 0 {
		t.Fatalf("clearing tags left %v", rec.Tags)
	}

	all, err := s.AllTags()
	if err != nil || len(all) != 0 {
		t.Fatalf("AllTags = %v, %v", all, err)
	}
}

// The reason the counters moved into the database: a removal must carry the
// torrent's lifetime bytes in the same transaction that drops its row.
func TestDeleteAbsorbIsAtomic(t *testing.T) {
	s, err := Open(filepath.Join(t.TempDir(), "hydra.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	if err := s.Put(&Record{InfoHash: "a", Session: Hoard, Torrent: []byte("x"),
		TotalUploaded: 1000, TotalDownloaded: 100}); err != nil {
		t.Fatal(err)
	}
	keys := []string{CounterGlobal, TrackerCounterKey("hoard", "tk.example.net")}
	if err := s.DeleteAbsorb("a", keys, 1000, 100); err != nil {
		t.Fatal(err)
	}
	if _, ok, _ := s.Get("a"); ok {
		t.Fatal("torrent still present after DeleteAbsorb")
	}
	for _, k := range keys {
		ul, dl, err := s.CounterGet(k)
		if err != nil || ul != 1000 || dl != 100 {
			t.Fatalf("counter %q = %d/%d (%v)", k, ul, dl, err)
		}
	}

	// Removing a second torrent accumulates rather than overwrites — this is
	// what keeps the per-tracker totals monotonic.
	if err := s.Put(&Record{InfoHash: "b", Session: Hoard, Torrent: []byte("y")}); err != nil {
		t.Fatal(err)
	}
	if err := s.DeleteAbsorb("b", keys, 5, 5); err != nil {
		t.Fatal(err)
	}
	if ul, dl, _ := s.CounterGet(CounterGlobal); ul != 1005 || dl != 105 {
		t.Fatalf("counters did not accumulate: %d/%d", ul, dl)
	}
}

func TestTrackerCounterKeyRoundTrip(t *testing.T) {
	k := TrackerCounterKey("race", "tk.tr4ker.net")
	eng, trk, ok := ParseTrackerCounterKey(k)
	if !ok || eng != "race" || trk != "tk.tr4ker.net" {
		t.Fatalf("round-trip: %q %q %v", eng, trk, ok)
	}
	if _, _, ok := ParseTrackerCounterKey(CounterGlobal); ok {
		t.Fatal("the global key must not parse as a tracker key")
	}
}

func TestMigrateSidecars(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "hydra.db")
	legacyDB(t, path,
		&Record{InfoHash: "a", Session: Hoard, Torrent: []byte("x")},
		&Record{InfoHash: "b", Session: Hoard, Torrent: []byte("y")},
	)

	write := func(name string, v any) {
		b, _ := json.Marshal(v)
		if err := os.WriteFile(filepath.Join(dir, name), b, 0o644); err != nil {
			t.Fatal(err)
		}
	}
	write("tags.json", map[string][]string{
		"a":    {"fr", "hd"},
		"gone": {"orphan"}, // a torrent removed long ago: must not resurrect
		"b":    {"vo"},
	})
	write("tags_registry.json", []string{"fr", "hd", "vo", "unused"})
	write("baseline.json", map[string]int64{"total_uploaded": 2558413332937930, "total_downloaded": 660139191874036})
	write("baseline_trackers.json", []map[string]any{
		{"engine": "hoard", "tracker": "tk.tr4ker.net", "ul": 3700000000000, "dl": 12},
	})

	s, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()

	rep := MigrateSidecars(dir, s)
	if len(rep.Errors) != 0 {
		t.Fatalf("import errors: %v", rep.Errors)
	}
	if rep.TaggedTorrents != 2 {
		t.Fatalf("tagged = %d, want 2 (the orphan must be dropped)", rep.TaggedTorrents)
	}

	all, _ := s.AllTags()
	if len(all) != 2 || len(all["a"]) != 2 || all["b"][0] != "vo" {
		t.Fatalf("tags not imported: %v", all)
	}
	if _, ok := all["gone"]; ok {
		t.Fatal("an orphan tag entry was resurrected")
	}
	names, _ := s.TagNames()
	if len(names) != 4 {
		t.Fatalf("registry = %v", names)
	}
	if ul, dl, _ := s.CounterGet(CounterGlobal); ul != 2558413332937930 || dl != 660139191874036 {
		t.Fatalf("global counter = %d/%d", ul, dl)
	}
	if ul, _, _ := s.CounterGet(TrackerCounterKey("hoard", "tk.tr4ker.net")); ul != 3700000000000 {
		t.Fatalf("tracker counter = %d", ul)
	}

	// Every sidecar is moved aside, never deleted.
	for _, n := range []string{"tags.json", "tags_registry.json", "baseline.json", "baseline_trackers.json"} {
		if _, err := os.Stat(filepath.Join(dir, n)); !os.IsNotExist(err) {
			t.Fatalf("%s should have been renamed aside", n)
		}
		if _, err := os.Stat(filepath.Join(dir, n+".migrated")); err != nil {
			t.Fatalf("%s.migrated missing — the original numbers must be recoverable", n)
		}
	}

	// A second boot finds nothing to do and changes nothing.
	rep2 := MigrateSidecars(dir, s)
	if !rep2.Empty() || len(rep2.Errors) != 0 {
		t.Fatalf("second run was not a no-op: %s %v", rep2, rep2.Errors)
	}
	if ul, _, _ := s.CounterGet(CounterGlobal); ul != 2558413332937930 {
		t.Fatalf("counters changed on re-run: %d", ul)
	}
}
