package api

import (
	"fmt"
	"path/filepath"

	"github.com/Kheopsian/hydra/internal/move"
)

// Working out where a torrent's payload should end up.
//
// Hydra keeps two notions of a path and they are not the same: the *content
// root* is the directory a torrent's data lives in, and the *engine save_path*
// is what Typhon is told -- the parent of the content root for a multi-file
// torrent, the content root itself otherwise. Both are computed here, once, so
// the two can never be derived differently in two places and disagree.

// movePlan is where a payload is, where it should go, and what the engine has
// to be told once it is there.
type movePlan struct {
	Source         string
	Target         string
	EngineSavePath string
	Name           string
	Host           engineHost

	// Loose marks the layout where the payload has no folder of its own:
	// its files sit directly in a category directory that holds other
	// torrents too. Source is then that shared directory and Files lists
	// what may leave it, because moving Source would take the category.
	Loose bool
	Files []string
}

// resolveMovePaths returns the current content root, the content root under
// the target category, and the engine save_path that goes with it.
func (s *Server) resolveMovePaths(infoHash, targetCategory string) (*movePlan, error) {
	host := s.hostHolding(infoHash)
	if host == nil {
		return nil, fmt.Errorf("no agent on this node holds %s", infoHash)
	}

	var detail map[string]interface{}
	var files []map[string]interface{}
	switch h := host.(type) {
	case HoardEngine:
		detail = h.GetTorrentDetail(infoHash)
		files = h.GetTorrentFileList(infoHash)
	case RaceEngine:
		detail = h.GetTorrentDetail(infoHash)
		files = h.GetTorrentFileList(infoHash)
	}
	if detail == nil {
		return nil, fmt.Errorf("torrent %s has no detail to move", infoHash)
	}

	src, _ := detail["save_path"].(string)
	name, _ := detail["name"].(string)
	if src == "" {
		return nil, fmt.Errorf("torrent %s has no save_path", infoHash)
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
		return nil, fmt.Errorf("category not found: %s", targetCategory)
	}
	if targetCatDir == "" {
		return nil, fmt.Errorf("category %s has no save_path", targetCategory)
	}

	// The loose layout: the content root IS a category directory, because the
	// payload was dropped straight into it -- the shape an rtorrent or
	// Transmission setup imports with. Moving that directory would move every
	// other torrent in the category, so the payload's own files move instead
	// and the directory is left where it is. The torrent keeps the loose
	// layout on the other side: changing a category should not silently
	// restructure somebody's library.
	if catDir := looseCategoryDir(src, cats); catDir != "" {
		rel, err := payloadRelPaths(files)
		if err != nil {
			return nil, fmt.Errorf("torrent %s has no folder of its own in %s, and %w", infoHash, catDir, err)
		}
		return &movePlan{
			Source:         filepath.Clean(src),
			Target:         filepath.Clean(targetCatDir),
			EngineSavePath: filepath.Clean(targetCatDir),
			Name:           name,
			Host:           host,
			Loose:          true,
			Files:          rel,
		}, nil
	}

	dst := filepath.Join(targetCatDir, filepath.Base(src))
	engineSavePath := dst
	if multi {
		engineSavePath = filepath.Dir(dst)
	}
	return &movePlan{
		Source:         filepath.Clean(src),
		Target:         filepath.Clean(dst),
		EngineSavePath: engineSavePath,
		Name:           name,
		Host:           host,
	}, nil
}

// looseCategoryDir returns the category directory a payload sits loose in, or
// "" when the payload has a folder of its own.
//
// The test is exact equality of the content root with a category's save path.
// A payload one level down has its own folder and moves normally; a payload
// whose root IS the category directory has nothing of its own to move.
func looseCategoryDir(src string, cats []category) string {
	clean := filepath.Clean(src)
	for _, cat := range cats {
		if cat.SavePath == "" {
			continue
		}
		if filepath.Clean(cat.SavePath) == clean {
			return clean
		}
	}
	return ""
}

// inspectFor plans the move the resolved paths describe.
func inspectFor(mp *movePlan, infoHash string) (*move.Plan, error) {
	if mp.Loose {
		return move.InspectLoose(mp.Source, mp.Target, infoHash, mp.Files)
	}
	return move.Inspect(mp.Source, mp.Target)
}

// payloadRelPaths pulls the torrent's own file list out of the engine's rows.
//
// A loose payload cannot be discovered by walking its directory -- that
// directory belongs to the category, and walking it would sweep up every other
// torrent in it. The torrent is the only thing that knows which files are its
// own, so no list means no move.
func payloadRelPaths(files []map[string]interface{}) ([]string, error) {
	var rel []string
	for _, f := range files {
		p, _ := f["path"].(string)
		if p == "" {
			continue
		}
		rel = append(rel, p)
	}
	if len(rel) == 0 {
		return nil, fmt.Errorf("the engine cannot say which files are its own, so they cannot be told apart from the rest of the category")
	}
	return rel, nil
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
