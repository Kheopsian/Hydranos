package engine

import (
	"os"
	"path/filepath"
	"testing"
)

// A routed add must leave the .torrent on disk at the content-addressed path
// the store reconcile reads back, or the torrent never reaches the store.
func TestMaterializeTorrentBlob(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "uploads")
	blob := []byte("d8:announce3:foo4:infod6:lengthi1e4:name1:xeee")

	ih, err := infoHashFromTorrentFile(blob)
	if err != nil {
		t.Fatalf("info_hash: %v", err)
	}

	path, err := MaterializeTorrentBlob(dir, blob)
	if err != nil {
		t.Fatalf("materialize: %v", err)
	}
	if want := filepath.Join(dir, ih+".torrent"); path != want {
		t.Fatalf("path = %q, want %q", path, want)
	}
	got, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read back: %v", err)
	}
	if string(got) != string(blob) {
		t.Fatalf("blob round-trip mismatch")
	}

	// Idempotent: the same torrent shipped twice reuses the same file.
	again, err := MaterializeTorrentBlob(dir, blob)
	if err != nil || again != path {
		t.Fatalf("second materialize = (%q, %v), want (%q, nil)", again, err, path)
	}
	ents, err := os.ReadDir(dir)
	if err != nil {
		t.Fatalf("readdir: %v", err)
	}
	if len(ents) != 1 {
		t.Fatalf("uploads dir has %d files, want 1", len(ents))
	}
}

func TestMaterializeTorrentBlobRejectsGarbage(t *testing.T) {
	if _, err := MaterializeTorrentBlob(t.TempDir(), []byte("not a torrent")); err == nil {
		t.Fatal("expected an error for bytes with no info dict")
	}
}
