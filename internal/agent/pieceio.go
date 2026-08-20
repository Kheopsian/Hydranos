package agent

import (
	"fmt"
	"os"

	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/Kheopsian/hydra/internal/pieceio"
)

// pieceContextFor resolves a torrent held by this agent: its metainfo, and the
// directory its files hang off. The placement logic itself lives in
// internal/pieceio, shared with the monolith's own engines -- two copies of
// "where does piece i go" would drift, and the bug would be silent corruption.
func (s *Server) pieceContextFor(c engine.EngineClient, infoHash string) (*pieceio.Context, error) {
	rec, err := c.ExportState(infoHash)
	if err != nil {
		return nil, fmt.Errorf("piece io: %w", err)
	}
	raw, err := os.ReadFile(rec.TorrentPath)
	if err != nil {
		return nil, fmt.Errorf("piece io: reading .torrent: %w", err)
	}
	return pieceio.New(raw, rec.SavePath)
}
