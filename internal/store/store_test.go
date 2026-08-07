package store

import (
	"path/filepath"
	"testing"
)

func openTmp(t *testing.T) *Store {
	t.Helper()
	s, err := Open(filepath.Join(t.TempDir(), "hydra.db"))
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	t.Cleanup(func() { s.Close() })
	return s
}

func TestPutGetDelete(t *testing.T) {
	s := openTmp(t)
	r := &Record{InfoHash: "aabb", Session: Hoard, Torrent: []byte("d4:infod...e"),
		SavePath: "/data/x", Category: "datafarm", AddedTime: 100, CompletedTime: 200,
		TotalUploaded: 5, TotalDownloaded: 6}
	if err := s.Put(r); err != nil {
		t.Fatalf("put: %v", err)
	}
	got, ok, err := s.Get("aabb")
	if err != nil || !ok {
		t.Fatalf("get: ok=%v err=%v", ok, err)
	}
	if got.SavePath != "/data/x" || got.Session != Hoard || string(got.Torrent) != "d4:infod...e" {
		t.Fatalf("roundtrip mismatch: %+v", got)
	}
	if _, ok, _ := s.Get("missing"); ok {
		t.Fatal("expected missing to be absent")
	}
	if err := s.Delete("aabb"); err != nil {
		t.Fatalf("delete: %v", err)
	}
	if _, ok, _ := s.Get("aabb"); ok {
		t.Fatal("expected deleted to be absent")
	}
}

// Adding the same info_hash twice must upsert, never create a duplicate or
// collide — this is the structural fix for the datafarm loss.
func TestUpsertNoCollision(t *testing.T) {
	s := openTmp(t)
	for i := 0; i < 3; i++ {
		if err := s.Put(&Record{InfoHash: "dup", Session: Hoard, Torrent: []byte("x"),
			Category: "c", SavePath: "/p"}); err != nil {
			t.Fatalf("put %d: %v", i, err)
		}
	}
	n, err := s.Count(Hoard)
	if err != nil {
		t.Fatalf("count: %v", err)
	}
	if n != 1 {
		t.Fatalf("expected 1 row after 3 upserts, got %d", n)
	}
}

func TestBySessionAndStats(t *testing.T) {
	s := openTmp(t)
	must := func(err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}
	must(s.Put(&Record{InfoHash: "r1", Session: Race, Torrent: []byte("a")}))
	must(s.Put(&Record{InfoHash: "h1", Session: Hoard, Torrent: []byte("b")}))
	must(s.Put(&Record{InfoHash: "h2", Session: Hoard, Torrent: []byte("c")}))

	hoard, err := s.BySession(Hoard)
	must(err)
	if len(hoard) != 2 {
		t.Fatalf("expected 2 hoard, got %d", len(hoard))
	}
	race, err := s.BySession(Race)
	must(err)
	if len(race) != 1 {
		t.Fatalf("expected 1 race, got %d", len(race))
	}

	must(s.UpdateStats("h1", 999, 111, 222))
	got, _, err := s.Get("h1")
	must(err)
	if got.CompletedTime != 999 || got.TotalUploaded != 111 || got.TotalDownloaded != 222 {
		t.Fatalf("stats not updated: %+v", got)
	}
	if string(got.Torrent) != "b" {
		t.Fatalf("UpdateStats must not touch blob, got %q", got.Torrent)
	}

	must(s.UpdatePlacement("h1", "movies", "/data/movies"))
	got, _, _ = s.Get("h1")
	if got.Category != "movies" || got.SavePath != "/data/movies" {
		t.Fatalf("placement not updated: %+v", got)
	}
}

func TestEmptyRejected(t *testing.T) {
	s := openTmp(t)
	if err := s.Put(&Record{InfoHash: "", Session: Hoard, Torrent: []byte("x")}); err == nil {
		t.Fatal("expected error on empty info_hash")
	}
	if err := s.Put(&Record{InfoHash: "z", Session: Hoard}); err == nil {
		t.Fatal("expected error on empty torrent blob")
	}
}

// Deleting a category must clear the label everywhere it is stored, across both
// sessions, and leave every other category alone (issue #7).
func TestClearCategory(t *testing.T) {
	s := openTmp(t)
	blob := []byte("d4:infod...e")
	for _, r := range []*Record{
		{InfoHash: "a1", Session: Hoard, Torrent: blob, Category: "films"},
		{InfoHash: "a2", Session: Race, Torrent: blob, Category: "films"},
		{InfoHash: "a3", Session: Hoard, Torrent: blob, Category: "series"},
		{InfoHash: "a4", Session: Hoard, Torrent: blob, Category: ""},
	} {
		if err := s.Put(r); err != nil {
			t.Fatalf("put %s: %v", r.InfoHash, err)
		}
	}
	n, err := s.ClearCategory("films")
	if err != nil {
		t.Fatalf("clear: %v", err)
	}
	if n != 2 {
		t.Fatalf("cleared %d rows, want 2 (both sessions)", n)
	}
	for _, ih := range []string{"a1", "a2"} {
		got, _, _ := s.Get(ih)
		if got.Category != "" {
			t.Fatalf("%s still carries %q", ih, got.Category)
		}
	}
	if got, _, _ := s.Get("a3"); got.Category != "series" {
		t.Fatalf("unrelated category clobbered: %q", got.Category)
	}
	// Idempotent, and an empty name is never a wildcard that wipes the column.
	if n, _ := s.ClearCategory("films"); n != 0 {
		t.Fatalf("second clear touched %d rows", n)
	}
	if n, _ := s.ClearCategory(""); n != 0 {
		t.Fatalf("empty category cleared %d rows", n)
	}
	if got, _, _ := s.Get("a3"); got.Category != "series" {
		t.Fatal("empty-category call wiped a real label")
	}
}
