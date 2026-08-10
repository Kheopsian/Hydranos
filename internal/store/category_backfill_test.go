package store

import (
	"path/filepath"
	"testing"
)

// catOf reads a row's category straight from SQL, so the assertions do not
// depend on the read path they are meant to check.
func catOf(t *testing.T, s *Store, infoHash string) string {
	t.Helper()
	var c string
	if err := s.db.QueryRow(
		`SELECT category FROM torrents WHERE info_hash = ?`, infoHash).Scan(&c); err != nil {
		t.Fatalf("read category %s: %v", infoHash, err)
	}
	return c
}

// openWith builds a store holding the given rows, in the shape a v3.50.0
// upgrade left behind: real torrents, category column empty.
func openWith(t *testing.T, rows ...*Record) *Store {
	t.Helper()
	path := filepath.Join(t.TempDir(), "hydra.db")
	s, err := Open(path)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	t.Cleanup(func() { s.Close() })
	for _, r := range rows {
		if err := s.Put(r); err != nil {
			t.Fatalf("put %s: %v", r.InfoHash, err)
		}
	}
	return s
}

func TestBackfillCategoriesRestoresDroppedCategories(t *testing.T) {
	s := openWith(t,
		&Record{InfoHash: "aa", Session: Hoard, Torrent: []byte("t")},
		&Record{InfoHash: "bb", Session: Hoard, Torrent: []byte("t")},
	)

	updated, ran, err := s.BackfillCategories(map[string]string{"aa": "Calewood", "bb": "MAM"})
	if err != nil {
		t.Fatalf("backfill: %v", err)
	}
	if !ran || updated != 2 {
		t.Fatalf("want ran=true updated=2, got ran=%v updated=%d", ran, updated)
	}
	if got := catOf(t, s, "aa"); got != "Calewood" {
		t.Errorf("aa: want Calewood, got %q", got)
	}
	if got := catOf(t, s, "bb"); got != "MAM" {
		t.Errorf("bb: want MAM, got %q", got)
	}
}

// The reason the marker exists. A user who deliberately clears a category must
// keep it cleared, and the retired state.json still says otherwise forever --
// so a repair keyed only on emptiness would put it back on every single boot.
func TestBackfillCategoriesRunsOnlyOnce(t *testing.T) {
	s := openWith(t, &Record{InfoHash: "aa", Session: Hoard, Torrent: []byte("t")})
	cats := map[string]string{"aa": "Calewood"}

	if _, ran, err := s.BackfillCategories(cats); err != nil || !ran {
		t.Fatalf("first run: ran=%v err=%v", ran, err)
	}

	// The user clears it by hand, the way the UI would.
	if err := s.UpdatePlacement("aa", "", "/data"); err != nil {
		t.Fatalf("clear category: %v", err)
	}

	updated, ran, err := s.BackfillCategories(cats)
	if err != nil {
		t.Fatalf("second run: %v", err)
	}
	if ran || updated != 0 {
		t.Fatalf("backfill ran twice: ran=%v updated=%d", ran, updated)
	}
	if got := catOf(t, s, "aa"); got != "" {
		t.Errorf("second run overwrote the user's choice: got %q, want empty", got)
	}
}

// Within the one run that is allowed, a torrent the user has already placed
// since the upgrade outranks whatever the retired state.json remembers.
func TestBackfillCategoriesKeepsPostUpgradePlacements(t *testing.T) {
	s := openWith(t,
		&Record{InfoHash: "aa", Session: Hoard, Torrent: []byte("t"), Category: "Race"},
		&Record{InfoHash: "bb", Session: Hoard, Torrent: []byte("t")},
	)

	updated, ran, err := s.BackfillCategories(map[string]string{"aa": "Calewood", "bb": "MAM"})
	if err != nil || !ran {
		t.Fatalf("backfill: ran=%v err=%v", ran, err)
	}
	if updated != 1 {
		t.Errorf("want 1 row updated, got %d", updated)
	}
	if got := catOf(t, s, "aa"); got != "Race" {
		t.Errorf("aa: backfill clobbered a post-upgrade placement: got %q", got)
	}
	if got := catOf(t, s, "bb"); got != "MAM" {
		t.Errorf("bb: want MAM, got %q", got)
	}
}

// A torrent the state file knows nothing about is left alone, and one the store
// no longer holds is not resurrected.
func TestBackfillCategoriesIgnoresUnknownTorrents(t *testing.T) {
	s := openWith(t, &Record{InfoHash: "aa", Session: Hoard, Torrent: []byte("t")})

	if _, _, err := s.BackfillCategories(map[string]string{"zz": "Calewood"}); err != nil {
		t.Fatalf("backfill: %v", err)
	}
	if got := catOf(t, s, "aa"); got != "" {
		t.Errorf("aa: want untouched, got %q", got)
	}
	var n int
	if err := s.db.QueryRow(`SELECT COUNT(*) FROM torrents`).Scan(&n); err != nil {
		t.Fatalf("count: %v", err)
	}
	if n != 1 {
		t.Errorf("backfill inserted rows: want 1, got %d", n)
	}
}
