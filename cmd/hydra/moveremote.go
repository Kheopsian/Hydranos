package main

import (
	"fmt"
	"os"
	"path/filepath"

	"github.com/Kheopsian/hydra/internal/drain"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
	"github.com/Kheopsian/hydra/internal/pieceio"
)

// localEndpoint lets THIS node be either end of a cross-node move.
//
// Without it a move is only ever agent-to-agent, which excludes the common
// case: a monolith (the front, holding the library) handing a torrent to an
// agent. It speaks the same EngineClient the agent server speaks remotely, so
// the two ends of a move are symmetric and the runner cannot tell them apart.
type localEndpoint struct {
	client  engine.EngineClient
	dataDir string
	// adopt registers the torrent with the Go layer as well as the engine.
	//
	// Going straight to the engine client leaves Hydra itself unaware of it:
	// no name, no save path, no event, so the row stays half-empty until
	// something reloads. The engine-level import is only half an adopt.
	adopt func(rec *ltclient.ResumeRecord) error
}

// category is baked in per job: the adopt path derives the on-disk layout from
// it, so handing it an empty string files the payload under the wrong category
// entirely -- which is how a duplicate landed in another category's directory
// with nothing in it.

func (l *localEndpoint) ExportState(infoHash string) (*ltclient.ResumeRecord, error) {
	return l.client.ExportState(infoHash)
}

func (l *localEndpoint) GetTorrentFile(infoHash string) ([]byte, error) {
	rec, err := l.client.ExportState(infoHash)
	if err != nil {
		return nil, err
	}
	return os.ReadFile(rec.TorrentPath)
}

// pieceCtx resolves where this torrent's bytes live right now.
//
// Rebuilt per call rather than cached: save_path can change under us (a local
// move, a category change), and a stale context would read or write the old
// location without complaining. See the note on transfer cost in the runner.
func (l *localEndpoint) pieceCtx(infoHash string) (*pieceio.Context, error) {
	rec, err := l.client.ExportState(infoHash)
	if err != nil {
		return nil, err
	}
	raw, err := os.ReadFile(rec.TorrentPath)
	if err != nil {
		return nil, fmt.Errorf("piece io: reading .torrent: %w", err)
	}
	return pieceio.New(raw, rec.SavePath)
}

func (l *localEndpoint) ReadPiece(infoHash string, piece int) ([]byte, error) {
	pc, err := l.pieceCtx(infoHash)
	if err != nil {
		return nil, err
	}
	return pc.ReadPiece(piece)
}

func (l *localEndpoint) WritePiece(infoHash string, piece int, data []byte) error {
	pc, err := l.pieceCtx(infoHash)
	if err != nil {
		return err
	}
	return pc.WritePiece(piece, data)
}

// ImportStateWithFile keeps our own copy of the .torrent and points the record
// at it, exactly as the agent server does: the incoming path names a file on
// the OTHER host.
func (l *localEndpoint) ImportStateWithFile(rec *ltclient.ResumeRecord, torrent []byte) (string, error) {
	if rec == nil {
		return "", fmt.Errorf("import: nil record")
	}
	dst := filepath.Join(l.dataDir, "move-import-"+rec.InfoHash+".torrent")
	if err := os.WriteFile(dst, torrent, 0644); err != nil {
		return "", fmt.Errorf("import: writing .torrent: %w", err)
	}
	rec2 := *rec
	rec2.TorrentPath = dst
	if l.adopt != nil {
		if err := l.adopt(&rec2); err != nil {
			os.Remove(dst)
			return "", err
		}
		return rec2.InfoHash, nil
	}
	ih, err := l.client.ImportState(&rec2)
	if err != nil {
		os.Remove(dst)
		return "", err
	}
	return ih, nil
}

func (l *localEndpoint) StopTorrent(infoHash string) error  { return l.client.StopTorrent(infoHash) }
func (l *localEndpoint) StartTorrent(infoHash string) error { return l.client.StartTorrent(infoHash) }
func (l *localEndpoint) VerifyTorrent(infoHash string) error {
	return l.client.VerifyTorrent(infoHash)
}

func (l *localEndpoint) RemoveTorrent(infoHash string, keepData bool) error {
	return l.client.RemoveTorrent(infoHash, keepData)
}

// localFreeSpace answers the preflight for this node.
func localFreeSpace(path string) (int64, error) {
	_, free, err := drain.TotalFree(path)
	return free, err
}
