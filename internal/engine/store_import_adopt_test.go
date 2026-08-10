package engine

import (
	"testing"
	"time"
)

// The engine rebuilds torrents from resume data, which carries no category and
// no save path. When the store import finds the torrent already there, it must
// hand those fields over rather than drop them -- otherwise the next sync
// writes the engine's blank view back over the store.
func TestAdoptStoreMetadataFillsWhatResumeDataLacks(t *testing.T) {
	fromResume := &TorrentInfo{InfoHash: "aa", Name: "Show S01"}
	wrapped := true
	done := time.Unix(1786268609, 0)

	adoptStoreMetadata(fromResume, &TorrentInfo{
		InfoHash:        "aa",
		Category:        "Calewood",
		SavePath:        "/data/downloads/calewood",
		TorrentFilePath: "/configs/uploads/abc",
		CompletedTime:   done,
		ContentFolder:   &wrapped,
	})

	if fromResume.Category != "Calewood" {
		t.Errorf("category: want Calewood, got %q", fromResume.Category)
	}
	if fromResume.SavePath != "/data/downloads/calewood" {
		t.Errorf("save path: got %q", fromResume.SavePath)
	}
	if fromResume.TorrentFilePath != "/configs/uploads/abc" {
		t.Errorf("torrent file: got %q", fromResume.TorrentFilePath)
	}
	if !fromResume.CompletedTime.Equal(done) {
		t.Errorf("completed time: got %v", fromResume.CompletedTime)
	}
	if fromResume.ContentFolder == nil || !*fromResume.ContentFolder {
		t.Errorf("content folder: got %v", fromResume.ContentFolder)
	}
	if fromResume.Name != "Show S01" {
		t.Errorf("name should survive: got %q", fromResume.Name)
	}
}

// A store that has nothing to say must not erase what the engine knows.
func TestAdoptStoreMetadataNeverBlanks(t *testing.T) {
	known := time.Unix(1786200000, 0)
	noWrapper := false
	live := &TorrentInfo{
		InfoHash:        "bb",
		Name:            "Film",
		Category:        "Race",
		SavePath:        "/race/torrents",
		TorrentFilePath: "/configs/uploads/xyz",
		CompletedTime:   known,
		ContentFolder:   &noWrapper,
	}

	adoptStoreMetadata(live, &TorrentInfo{InfoHash: "bb"})

	if live.Category != "Race" {
		t.Errorf("category blanked: got %q", live.Category)
	}
	if live.SavePath != "/race/torrents" {
		t.Errorf("save path blanked: got %q", live.SavePath)
	}
	if live.TorrentFilePath != "/configs/uploads/xyz" {
		t.Errorf("torrent file blanked: got %q", live.TorrentFilePath)
	}
	if !live.CompletedTime.Equal(known) {
		t.Errorf("completed time blanked: got %v", live.CompletedTime)
	}
	if live.ContentFolder == nil || *live.ContentFolder {
		t.Errorf("content folder blanked: got %v", live.ContentFolder)
	}
}
