package api

import (
	"fmt"
	"path/filepath"
)

// Working out where a torrent's payload should end up.
//
// Hydra keeps two notions of a path and they are not the same: the *content
// root* is the directory a torrent's data lives in, and the *engine save_path*
// is what Typhon is told -- the parent of the content root for a multi-file
// torrent, the content root itself otherwise. Both are computed here, once, so
// the two can never be derived differently in two places and disagree.

// resolveMovePaths returns the current content root, the content root under
// the target category, and the engine save_path that goes with it.
func (s *Server) resolveMovePaths(infoHash, targetCategory string) (src, dst, engineSavePath, name string, host engineHost, err error) {
	host = s.hostHolding(infoHash)
	if host == nil {
		return "", "", "", "", nil, fmt.Errorf("no agent on this node holds %s", infoHash)
	}

	var detail map[string]interface{}
	switch h := host.(type) {
	case HoardEngine:
		detail = h.GetTorrentDetail(infoHash)
	case RaceEngine:
		detail = h.GetTorrentDetail(infoHash)
	}
	if detail == nil {
		return "", "", "", "", nil, fmt.Errorf("torrent %s has no detail to move", infoHash)
	}

	src, _ = detail["save_path"].(string)
	name, _ = detail["name"].(string)
	if src == "" {
		return "", "", "", "", nil, fmt.Errorf("torrent %s has no save_path", infoHash)
	}
	multi, _ := detail["multi_file"].(bool)

	cats := loadCategories(s.config.Daemon.DataDir)
	targetCatDir := ""
	found := false
	for _, cat := range cats {
		if cat.Name == targetCategory {
			targetCatDir = cat.SavePath
			found = true
			break
		}
	}
	if !found {
		return "", "", "", "", nil, fmt.Errorf("category not found: %s", targetCategory)
	}
	if targetCatDir == "" {
		return "", "", "", "", nil, fmt.Errorf("category %s has no save_path", targetCategory)
	}

	// Refuse the loose layout, where the payload is a bare file sitting
	// directly in the category directory and the content root IS that
	// directory. Moving it would mean moving the whole category -- every
	// other torrent in it included. The existing rename path handles this
	// case specially; until the mover does too, refusing is the only honest
	// answer.
	for _, cat := range cats {
		if cat.SavePath != "" && filepath.Clean(cat.SavePath) == filepath.Clean(src) {
			return "", "", "", "", nil, fmt.Errorf(
				"torrent %s has no folder of its own (its data sits loose in category directory %s); "+
					"moving it would move the whole category", infoHash, src)
		}
	}

	dst = filepath.Join(targetCatDir, filepath.Base(src))
	if multi {
		engineSavePath = filepath.Dir(dst)
	} else {
		engineSavePath = dst
	}
	return filepath.Clean(src), filepath.Clean(dst), engineSavePath, name, host, nil
}

// hostHolding returns the engine that currently holds the torrent, or nil.
func (s *Server) hostHolding(infoHash string) engineHost {
	if s.hoardEngine != nil && s.hoardEngine.HasTorrent(infoHash) {
		return s.hoardEngine
	}
	if s.raceEngine != nil && s.raceEngine.HasTorrent(infoHash) {
		return s.raceEngine
	}
	return nil
}
