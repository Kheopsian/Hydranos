// Package tagstore persists per-torrent tags (qBittorrent-style multi-labels) to
// a tags.json overlay in the data dir. Tags are non-critical labels, so they
// live here rather than in the SQLite identity store: losing them on a crash is
// an annoyance, not a lost torrent, and this keeps the store schema untouched.
package tagstore

import (
	"encoding/json"
	"os"
	"path/filepath"
)

// Path returns the tags.json path inside dataDir.
func Path(dataDir string) string { return filepath.Join(dataDir, "tags.json") }

// Load reads the info_hash -> tags map (empty map on any error / missing file).
func Load(dataDir string) map[string][]string {
	m := map[string][]string{}
	b, err := os.ReadFile(Path(dataDir))
	if err != nil {
		return m
	}
	_ = json.Unmarshal(b, &m)
	return m
}

// Save writes the map atomically (tmp + rename).
func Save(dataDir string, m map[string][]string) error {
	b, err := json.MarshalIndent(m, "", "  ")
	if err != nil {
		return err
	}
	tmp := Path(dataDir) + ".tmp"
	if err := os.WriteFile(tmp, b, 0o644); err != nil {
		return err
	}
	return os.Rename(tmp, Path(dataDir))
}
