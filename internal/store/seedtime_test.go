package store

import (
	"os"
	"path/filepath"
	"testing"
)

// The seed time column round-trips through a sync and is MONOTONE: an engine
// that has not loaded a torrent yet reports 0 for it, and a blind write would
// erase weeks of retention history on the first tick after a restart.
func TestSyncSeedTimeIsMonotone(t *testing.T) {
	dir := t.TempDir()
	blob := filepath.Join(dir, "a.torrent")
	if err := os.WriteFile(blob, []byte("d1e"), 0644); err != nil {
		t.Fatal(err)
	}
	s, err := Open(filepath.Join(dir, "hydra.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()

	item := SyncItem{InfoHash: "a", Session: Hoard, TorrentFilePath: blob, SeedingTime: 3600}
	if _, err := s.SyncAll([]SyncItem{item}); err != nil {
		t.Fatal(err)
	}
	all, err := s.AllSeedTimes()
	if err != nil {
		t.Fatal(err)
	}
	if all["a"] != 3600 {
		t.Fatalf("seed time = %d, want 3600 after insert", all["a"])
	}

	// A later sync reporting less must not lower it.
	item.SeedingTime = 0
	if _, err := s.SyncAll([]SyncItem{item}); err != nil {
		t.Fatal(err)
	}
	all, _ = s.AllSeedTimes()
	if all["a"] != 3600 {
		t.Fatalf("seed time = %d, want 3600 (a 0 report must not erase it)", all["a"])
	}

	// And a higher one advances it.
	item.SeedingTime = 7200
	if _, err := s.SyncAll([]SyncItem{item}); err != nil {
		t.Fatal(err)
	}
	all, _ = s.AllSeedTimes()
	if all["a"] != 7200 {
		t.Fatalf("seed time = %d, want 7200", all["a"])
	}
}
