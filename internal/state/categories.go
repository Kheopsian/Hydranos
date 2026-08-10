package state

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

// CategoriesFrom reads only the per-torrent categories out of a state file.
//
// The path is usually state.json.migrated: once the store took over, the
// retired file became the only place the categories of pre-v3.50.0 torrents
// still exist. Reading it whole and keeping one field is cheap enough at boot,
// and it avoids a second parser drifting from the real shape.
func CategoriesFrom(path string) (map[string]string, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var st State
	if err := json.Unmarshal(data, &st); err != nil {
		return nil, fmt.Errorf("parse %s: %w", filepath.Base(path), err)
	}
	return categoriesOf(st.Race, st.HoardActive), nil
}

// categoriesOf flattens the per-session maps into infohash -> category, keeping
// only the torrents that actually carry one.
func categoriesOf(sessions ...map[string]*TorrentMeta) map[string]string {
	out := make(map[string]string)
	for _, sess := range sessions {
		for infoHash, meta := range sess {
			if meta != nil && meta.Category != "" {
				out[infoHash] = meta.Category
			}
		}
	}
	return out
}

// Categories returns the per-torrent categories held in an already-loaded
// state, for the upgrade that still has its state.json in hand.
func (s *State) Categories() map[string]string {
	if s == nil {
		return nil
	}
	return categoriesOf(s.Race, s.HoardActive)
}
