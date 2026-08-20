package engine

import (
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"time"

	"github.com/Kheopsian/hydra/internal/engine/ltclient"
)

// Moving a torrent from one engine to another.
//
// A torrent's identity lives in the store and its progression lives in the
// engine that holds it, so handing one over means moving both halves without
// ever having them disagree. The order below is the whole design: the target
// adopts first and the source lets go last, so an interruption anywhere leaves
// the torrent seeding from two engines -- visible, harmless, fixable -- rather
// than from none.
//
// What this deliberately does NOT do is move payload files. Race data lives on
// the NVMe and hoard data on the array, so a race->hoard file move is a
// cross-filesystem copy of possibly hundreds of gigabytes, which is a
// different feature with different failure modes (partial copies, hardlinks
// broken for the *arr stack, disk filling mid-move). The handoff leaves the
// bytes exactly where they are and points the new engine at them.

// TorrentHost is the part of an engine that can give up a torrent or take one
// on. Both roles implement it; the orchestrator does not care which is which.
type TorrentHost interface {
	Role() Role
	HasTorrent(infoHash string) bool
	ExportTorrentState(infoHash string) (*ltclient.ResumeRecord, error)
	AdoptTorrent(rec *ltclient.ResumeRecord, category string) error
	ReleaseTorrent(infoHash string) error
}

var (
	_ TorrentHost = (*RaceEngine)(nil)
	_ TorrentHost = (*HoardEngine)(nil)
)

// MoveTorrent hands one torrent from src to dst, progression included.
//
// Returns an error and leaves the torrent untouched in src if anything fails
// before the adoption succeeds. Once dst has adopted, a failure to release
// from src is logged rather than fatal: the torrent is already correctly
// seeding from its new home, and a stale entry in the old one is a smaller
// problem than unwinding a completed adoption.
func MoveTorrent(src, dst TorrentHost, infoHash, category string) error {
	if src == nil || dst == nil {
		return fmt.Errorf("move: source or target engine unavailable")
	}
	if src.Role() == dst.Role() {
		return fmt.Errorf("move: source and target are both %s", src.Role())
	}
	if !src.HasTorrent(infoHash) {
		return fmt.Errorf("move: torrent not held by the %s engine", src.Role())
	}
	if dst.HasTorrent(infoHash) {
		return fmt.Errorf("move: torrent is already in the %s engine", dst.Role())
	}

	rec, err := src.ExportTorrentState(infoHash)
	if err != nil {
		return fmt.Errorf("move: export from %s: %w", src.Role(), err)
	}
	// The .torrent file itself has to be readable from the target's side. It
	// is a path in a shared config directory today, but checking here turns a
	// silent half-move into a refusal before anything has changed.
	if _, err := os.Stat(rec.TorrentPath); err != nil {
		return fmt.Errorf("move: torrent file unreadable at %s: %w", rec.TorrentPath, err)
	}
	if _, err := os.Stat(rec.SavePath); err != nil {
		return fmt.Errorf("move: data path unreachable at %s: %w", rec.SavePath, err)
	}

	if err := dst.AdoptTorrent(rec, category); err != nil {
		return fmt.Errorf("move: adopt into %s: %w", dst.Role(), err)
	}

	if err := src.ReleaseTorrent(infoHash); err != nil {
		slog.Error("move: target adopted but source would not let go -- torrent is now in both engines",
			"info_hash", infoHash, "from", src.Role(), "to", dst.Role(), "error", err)
		return nil
	}
	slog.Info("move: torrent handed over",
		"info_hash", infoHash, "from", src.Role(), "to", dst.Role(), "category", category)
	return nil
}

// ---- HoardEngine ----

func (e *HoardEngine) ExportTorrentState(infoHash string) (*ltclient.ResumeRecord, error) {
	return e.client.ExportState(infoHash)
}

func (e *HoardEngine) AdoptTorrent(rec *ltclient.ResumeRecord, category string) error {
	if !e.running {
		return fmt.Errorf("hoard: engine not running")
	}
	onDisk, name, cf, err := adoptedLayout(rec)
	if err != nil {
		return err
	}
	if _, err := e.client.ImportState(rec); err != nil {
		return fmt.Errorf("hoard: import state: %w", err)
	}
	info := &TorrentInfo{
		InfoHash:        rec.InfoHash,
		Name:            name,
		SavePath:        onDisk,
		Category:        category,
		AddedTime:       time.Unix(rec.AddedTime, 0),
		TorrentFilePath: rec.TorrentPath,
		ContentFolder:   cf,
	}
	if rec.CompletedTime > 0 {
		info.CompletedTime = time.Unix(rec.CompletedTime, 0)
	}
	e.mu.Lock()
	e.torrents[rec.InfoHash] = info
	e.mu.Unlock()

	// Same reason the add path seeds these: until the next refreshStats tick
	// the qBit shim would otherwise derive the path from the Go save_path and
	// double the release directory for a multi-file torrent.
	e.cachedStatsMu.Lock()
	st, ok := e.cachedStats[rec.InfoHash]
	if !ok {
		st = &TorrentStats{InfoHash: rec.InfoHash}
		e.cachedStats[rec.InfoHash] = st
	}
	st.MultiFile = cf != nil && *cf
	st.EngineSavePath = rec.SavePath
	st.SavePath = onDisk
	st.Category = category
	e.cachedStatsMu.Unlock()

	// A torrent that just changed engine has been out of its swarm for the
	// duration of the handover, and the tracker was told "stopped" by the
	// source. Without this it stays at zero seeders until the slow hoard
	// announce cycle comes round -- the same trap the category rename hit.
	if e.reAnnounce != nil {
		go e.reAnnounce(rec.InfoHash, 0)
	}
	return nil
}

func (e *HoardEngine) ReleaseTorrent(infoHash string) error {
	e.RemoveTorrent(infoHash, true)
	return nil
}

// ---- RaceEngine ----

func (e *RaceEngine) ExportTorrentState(infoHash string) (*ltclient.ResumeRecord, error) {
	return e.client.ExportState(infoHash)
}

func (e *RaceEngine) AdoptTorrent(rec *ltclient.ResumeRecord, category string) error {
	onDisk, name, cf, err := adoptedLayout(rec)
	if err != nil {
		return err
	}
	if _, err := e.client.ImportState(rec); err != nil {
		return fmt.Errorf("race: import state: %w", err)
	}
	info := &TorrentInfo{
		InfoHash:        rec.InfoHash,
		Name:            name,
		SavePath:        onDisk,
		Category:        category,
		AddedTime:       time.Unix(rec.AddedTime, 0),
		TorrentFilePath: rec.TorrentPath,
		ContentFolder:   cf,
	}
	if rec.CompletedTime > 0 {
		info.CompletedTime = time.Unix(rec.CompletedTime, 0)
	}
	e.mu.Lock()
	e.torrents[rec.InfoHash] = info
	e.addedTime[rec.InfoHash] = info.AddedTime
	e.mu.Unlock()
	return nil
}

func (e *RaceEngine) ReleaseTorrent(infoHash string) error {
	return e.RemoveTorrent(infoHash, true)
}

// adoptedLayout re-derives the Go-side path shape from the .torrent the record
// points at.
//
// The record carries the *engine* save_path, which for a multi-file torrent is
// the parent of the content root; Hydra's own bookkeeping wants the content
// root itself. Rather than transmit both and risk them disagreeing, the shape
// is recomputed from the torrent file, exactly as the add path computes it.
func adoptedLayout(rec *ltclient.ResumeRecord) (onDisk, name string, contentFolder *bool, err error) {
	data, err := os.ReadFile(rec.TorrentPath)
	if err != nil {
		return "", "", nil, fmt.Errorf("adopt: read torrent file: %w", err)
	}
	name = nameFromTorrentFile(data)
	if name == "" {
		return "", "", nil, fmt.Errorf("adopt: torrent file has no name: %s", rec.TorrentPath)
	}
	multi := isMultiFileTorrent(data)
	if multi {
		onDisk = filepath.Join(rec.SavePath, name)
	} else {
		onDisk = rec.SavePath
	}
	contentFolder = &multi
	return onDisk, name, contentFolder, nil
}
