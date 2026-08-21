package engine

import "testing"

// The shim answers per-category polls; filtering inside the engine keeps the
// copy of every other torrent's struct off that path entirely.
func TestGetTorrentListInCategory(t *testing.T) {
	e := &HoardEngine{cachedStats: map[string]*TorrentStats{
		"a": {InfoHash: "a", Category: "movies"},
		"b": {InfoHash: "b", Category: "series"},
		"c": {InfoHash: "c", Category: "movies"},
		"d": {InfoHash: "d", Category: ""},
	}}

	got := e.GetTorrentListInCategory("movies")
	if len(got) != 2 {
		t.Fatalf("got %d torrents, want 2", len(got))
	}
	for _, s := range got {
		if s.Category != "movies" {
			t.Errorf("leaked a %q torrent", s.Category)
		}
	}
	if n := len(e.GetTorrentListInCategory("nope")); n != 0 {
		t.Errorf("unknown category returned %d torrents", n)
	}
	// The uncategorised torrent is reachable, and only, by the empty category.
	if n := len(e.GetTorrentListInCategory("")); n != 1 {
		t.Errorf("empty category returned %d torrents, want 1", n)
	}
	// Copies, not aliases: the caller must not be able to write into the cache.
	got[0].Category = "mutated"
	if e.cachedStats[got[0].InfoHash].Category != "movies" {
		t.Error("caller writes reached the engine's cached stats")
	}
}
