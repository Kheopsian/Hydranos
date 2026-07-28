package store

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	"github.com/Kheopsian/hydra/internal/state"
)

func TestImportLegacy(t *testing.T) {
	dir := t.TempDir()

	// A real .torrent blob on disk for h1; h2 points at a missing file.
	tfile := filepath.Join(dir, "h1.torrent")
	if err := os.WriteFile(tfile, []byte("d4:infod6:lengthi1eee"), 0644); err != nil {
		t.Fatal(err)
	}

	st := state.State{
		Version: 1,
		HoardActive: map[string]*state.TorrentMeta{
			"h1": {TorrentFilePath: tfile, SavePath: "/data/h1", Category: "datafarm",
				AddedTime: 10, CompletedTime: 20, TotalUploaded: 3, TotalDownloaded: 4},
			"h2": {TorrentFilePath: filepath.Join(dir, "gone.torrent"), Category: "x"},
			"h3": {TorrentFilePath: ""}, // no path at all
		},
		Race: map[string]*state.TorrentMeta{
			"r1": {TorrentFilePath: tfile, SavePath: "/data/r1"},
		},
	}
	statePath := filepath.Join(dir, "state.json")
	b, _ := json.Marshal(st)
	if err := os.WriteFile(statePath, b, 0644); err != nil {
		t.Fatal(err)
	}

	s, err := Open(filepath.Join(dir, "hydra.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()

	res, err := s.ImportLegacy(statePath)
	if err != nil {
		t.Fatalf("import: %v", err)
	}
	if res.Imported != 2 { // h1 + r1
		t.Fatalf("expected 2 imported, got %d", res.Imported)
	}
	if res.MissingFile != 2 { // h2 (gone) + h3 (no path)
		t.Fatalf("expected 2 missing, got %d", res.MissingFile)
	}

	got, ok, _ := s.Get("h1")
	if !ok || got.Category != "datafarm" || got.CompletedTime != 20 ||
		string(got.Torrent) != "d4:infod6:lengthi1eee" || got.Session != Hoard {
		t.Fatalf("h1 roundtrip wrong: %+v ok=%v", got, ok)
	}
	if n, _ := s.Count(Race); n != 1 {
		t.Fatalf("expected 1 race, got %d", n)
	}

	// Idempotent: second run upserts, no duplication.
	if _, err := s.ImportLegacy(statePath); err != nil {
		t.Fatalf("reimport: %v", err)
	}
	if n, _ := s.Count(Hoard); n != 1 {
		t.Fatalf("expected 1 hoard after reimport, got %d", n)
	}
}
