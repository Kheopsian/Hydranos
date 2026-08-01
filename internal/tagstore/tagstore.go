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

// RegistryPath returns the tags_registry.json path: the set of known tag names,
// including tags created but not yet assigned (qBittorrent parity).
func RegistryPath(dataDir string) string { return filepath.Join(dataDir, "tags_registry.json") }

// LoadRegistry reads the known-tag name list (nil on missing/error).
func LoadRegistry(dataDir string) []string {
	var out []string
	b, err := os.ReadFile(RegistryPath(dataDir))
	if err != nil {
		return out
	}
	_ = json.Unmarshal(b, &out)
	return out
}

// SaveRegistry writes the known-tag name list atomically.
func SaveRegistry(dataDir string, names []string) error {
	b, err := json.MarshalIndent(names, "", "  ")
	if err != nil {
		return err
	}
	tmp := RegistryPath(dataDir) + ".tmp"
	if err := os.WriteFile(tmp, b, 0o644); err != nil {
		return err
	}
	return os.Rename(tmp, RegistryPath(dataDir))
}
