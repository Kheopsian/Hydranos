package store

import (
	"path/filepath"
	"testing"
)

// What a category's torrents are already doing is what re-creating it should
// start from. The form opens on race with an empty path, so a label whose
// torrents are all in hoard would be adopted straight into the wrong engine --
// a reclassification nobody asked for and nothing announces.
func TestCategoryCountsReportsWhereTheTorrentsActuallyAre(t *testing.T) {
	s, err := Open(filepath.Join(t.TempDir(), "test.db"))
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer s.Close()

	put := func(ih string, sess Session, path, cat string) {
		t.Helper()
		if err := s.Put(&Record{
			InfoHash: ih,
			Session:  sess,
			Torrent:  []byte("x"),
			SavePath: path,
			Category: cat,
		}); err != nil {
			t.Fatal(err)
		}
	}

	// Mostly hoard under /data/tv, with a stray race one elsewhere: the
	// majority is the answer, not the first row the database happens to return.
	put("a1", Hoard, "/data/tv", "imported")
	put("a2", Hoard, "/data/tv", "imported")
	put("a3", Hoard, "/data/tv", "imported")
	put("a4", Race, "/race/scratch", "imported")

	// A category whose commonest path is empty: an empty proposal helps nobody,
	// so a real directory from a minority must win over a blank majority.
	put("b1", Race, "", "blanks")
	put("b2", Race, "", "blanks")
	put("b3", Race, "/race/keep", "blanks")

	counts, err := s.CategoryCounts()
	if err != nil {
		t.Fatalf("counts: %v", err)
	}
	got := map[string]CategoryCount{}
	for _, cc := range counts {
		got[cc.Name] = cc
	}

	imp, ok := got["imported"]
	if !ok {
		t.Fatal("imported missing from the counts")
	}
	if imp.Count != 4 {
		t.Errorf("count = %d, want 4", imp.Count)
	}
	if imp.Session != Hoard {
		t.Errorf("session = %q, want hoard: adopting this would move four torrents to race", imp.Session)
	}
	if imp.SavePath != "/data/tv" {
		t.Errorf("save_path = %q, want /data/tv", imp.SavePath)
	}

	bl, ok := got["blanks"]
	if !ok {
		t.Fatal("blanks missing from the counts")
	}
	if bl.SavePath != "/race/keep" {
		t.Errorf("save_path = %q, want /race/keep: an empty path must never win the vote", bl.SavePath)
	}
	if bl.Count != 3 {
		t.Errorf("count = %d, want 3", bl.Count)
	}
}
