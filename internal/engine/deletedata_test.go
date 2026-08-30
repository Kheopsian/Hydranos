package engine

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

func writeFileAt(t *testing.T, path string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
}

func mustStillExist(t *testing.T, path, why string) {
	t.Helper()
	if _, err := os.Lstat(path); err != nil {
		t.Fatalf("%s: %s is gone (%v)", why, path, err)
	}
}

func mustBeGone(t *testing.T, path, why string) {
	t.Helper()
	if _, err := os.Lstat(path); err == nil {
		t.Fatalf("%s: %s should have been deleted", why, path)
	}
}

// The one that matters: a multi-file torrent whose info.name is also a
// directory holding somebody else's data. os.RemoveAll on savePath/name took
// both down; drain calls this in a loop with deleteFiles=true, so no human is
// in the way.
func TestRemoveTorrentFilesSparesForeignDataUnderTheSameName(t *testing.T) {
	save := t.TempDir()
	name := "Pack.2026"

	ours := filepath.Join(save, name, "cd1", "ours.bin")
	foreign := filepath.Join(save, name, "someone.elses.mkv")
	writeFileAt(t, ours)
	writeFileAt(t, foreign)

	removed, err := removeTorrentFiles(save, name, []ltclient.FileInfo{
		{Path: filepath.Join("cd1", "ours.bin"), Size: 1},
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if removed != 1 {
		t.Fatalf("expected 1 file removed, got %d", removed)
	}

	mustBeGone(t, ours, "the torrent's own file")
	mustStillExist(t, foreign, "a file this torrent does not own was deleted")
	mustStillExist(t, filepath.Join(save, name), "the shared directory was removed while it still held foreign data")
}

// With no foreign data the directory must not survive as an empty husk.
func TestRemoveTorrentFilesPrunesTheDirectoryItEmptied(t *testing.T) {
	save := t.TempDir()
	name := "Pack.2026"
	writeFileAt(t, filepath.Join(save, name, "cd1", "a.bin"))
	writeFileAt(t, filepath.Join(save, name, "cd2", "b.bin"))

	removed, err := removeTorrentFiles(save, name, []ltclient.FileInfo{
		{Path: filepath.Join("cd1", "a.bin")},
		{Path: filepath.Join("cd2", "b.bin")},
	})
	if err != nil || removed != 2 {
		t.Fatalf("removed=%d err=%v", removed, err)
	}
	mustBeGone(t, filepath.Join(save, name), "the emptied torrent directory")
}

func TestRemoveTorrentFilesSingleFileLayout(t *testing.T) {
	save := t.TempDir()
	name := "movie.mkv"
	writeFileAt(t, filepath.Join(save, name))
	neighbour := filepath.Join(save, "other.mkv")
	writeFileAt(t, neighbour)

	removed, err := removeTorrentFiles(save, name, []ltclient.FileInfo{{Path: name}})
	if err != nil || removed != 1 {
		t.Fatalf("removed=%d err=%v", removed, err)
	}
	mustBeGone(t, filepath.Join(save, name), "the single-file payload")
	mustStillExist(t, neighbour, "a flat save_path neighbour was deleted")
	mustStillExist(t, save, "save_path itself was removed")
}

// No file list means no proof of ownership. Deleting on savePath/name alone is
// the guess this function exists to refuse.
func TestRemoveTorrentFilesWithoutAListDeletesNothing(t *testing.T) {
	save := t.TempDir()
	name := "Pack.2026"
	victim := filepath.Join(save, name, "data.bin")
	writeFileAt(t, victim)

	removed, err := removeTorrentFiles(save, name, nil)
	if err != nil || removed != 0 {
		t.Fatalf("removed=%d err=%v", removed, err)
	}
	mustStillExist(t, victim, "data was deleted with no file list to justify it")
}

// A .torrent is attacker-controlled input.
func TestRemoveTorrentFilesRefusesPathsThatEscapeSavePath(t *testing.T) {
	save := t.TempDir()
	outside := filepath.Join(save, "..", "outside.bin")
	writeFileAt(t, outside)
	t.Cleanup(func() { os.Remove(outside) })

	name := "Pack.2026"
	writeFileAt(t, filepath.Join(save, name, "keep.bin"))

	removed, err := removeTorrentFiles(save, name, []ltclient.FileInfo{
		{Path: filepath.Join("..", "..", "outside.bin")},
	})
	if err == nil {
		t.Fatal("a path escaping save_path must be reported, not silently skipped")
	}
	if removed != 0 {
		t.Fatalf("removed=%d, nothing should have been deleted", removed)
	}
	mustStillExist(t, outside, "a traversal path reached outside save_path")
}
