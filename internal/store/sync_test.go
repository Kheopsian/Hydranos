package store

import (
	"os"
	"path/filepath"
	"testing"
)

func TestSyncAll(t *testing.T) {
	dir := t.TempDir()
	blob := filepath.Join(dir, "a.torrent")
	if err := os.WriteFile(blob, []byte("d...e"), 0644); err != nil {
		t.Fatal(err)
	}
	cblob := filepath.Join(dir, "c.torrent")
	if err := os.WriteFile(cblob, []byte("d2e"), 0644); err != nil {
		t.Fatal(err)
	}
	s, err := Open(filepath.Join(dir, "hydra.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()

	// Round 1: hoard has a+b (b blob missing), race has c.
	r, err := s.SyncAll([]SyncItem{
		{InfoHash: "a", Session: Hoard, TorrentFilePath: blob, Category: "c1", CompletedTime: 5},
		{InfoHash: "b", Session: Hoard, TorrentFilePath: filepath.Join(dir, "gone.torrent")},
		{InfoHash: "c", Session: Race, TorrentFilePath: cblob, Category: "cr"},
	})
	if err != nil {
		t.Fatalf("sync1: %v", err)
	}
	if r.Inserted != 2 || r.Missing != 1 {
		t.Fatalf("round1 want ins=2 miss=1, got %+v", r)
	}

	// Round 2: THE PROD BUG — 'c' now reported by BOTH race and hoard.
	// Must not fail on the global UNIQUE(info_hash); last wins, counted Conflict.
	r, err = s.SyncAll([]SyncItem{
		{InfoHash: "a", Session: Hoard, TorrentFilePath: blob, Category: "c1", CompletedTime: 5},
		{InfoHash: "c", Session: Race, TorrentFilePath: cblob, Category: "cr"},
		{InfoHash: "c", Session: Hoard, TorrentFilePath: cblob, Category: "ch"},
	})
	if err != nil {
		t.Fatalf("sync2 (cross-session) must not fail: %v", err)
	}
	if r.Conflicts != 1 {
		t.Fatalf("round2 want conflicts=1, got %+v", r)
	}
	got, _, _ := s.Get("c")
	if got == nil || got.Session != Hoard { // last item wins
		t.Fatalf("c should have flipped to hoard, got %+v", got)
	}

	// Round 3: hoard reports nothing (mid-restart), race still has c.
	// Hoard rows (a) must NOT be wiped.
	r, err = s.SyncAll([]SyncItem{
		{InfoHash: "c", Session: Race, TorrentFilePath: cblob, Category: "cr"},
	})
	if err != nil {
		t.Fatalf("sync3: %v", err)
	}
	if _, ok, _ := s.Get("a"); !ok {
		t.Fatal("hoard 'a' wrongly wiped when hoard reported nothing")
	}
	if got, _, _ := s.Get("c"); got.Session != Race {
		t.Fatalf("c should be race again, got %+v", got)
	}

	// Round 4: empty everything must refuse to wipe.
	if _, err := s.SyncAll(nil); err == nil {
		t.Fatal("expected refusal to wipe on empty report")
	}
}
