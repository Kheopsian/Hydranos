package api

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
)

// A payload sitting loose in the category directory has that directory as its
// content root, so moving it takes every other torrent in the category along.
// The agent cannot see this -- categories live on the front -- so the check is
// made here, against the paths THAT agent uses.
func TestALoosePayloadIsRefusedPerAgentPaths(t *testing.T) {
	dir := t.TempDir()
	// The on-disk format is a map keyed by name, not a list.
	cats := map[string]map[string]any{
		"movies": {"save_path": "/config/tr-data/movies",
			"agents": map[string]string{"heracles": `C:\Hydra\movies`}},
		"series": {"save_path": "/config/tr-data/series"},
	}
	blob, _ := json.Marshal(cats)
	if err := os.WriteFile(filepath.Join(dir, "categories.json"), blob, 0644); err != nil {
		t.Fatal(err)
	}
	cfg := &config.HydraConfig{}
	cfg.Daemon.DataDir = dir
	s := &Server{config: cfg}

	// An engine of this machine uses the plain save path.
	if why := s.looseInCategoryDir("local-vpn7", "/config/tr-data/movies"); why == "" {
		t.Error("a payload rooted at this node's category directory was allowed to move")
	}
	// A torrent with a folder of its own is fine.
	if why := s.looseInCategoryDir("local-vpn7", "/config/tr-data/movies/Some.Release"); why != "" {
		t.Errorf("a torrent with its own folder was refused: %s", why)
	}
	// A remote agent is measured against ITS mapping, not ours. Comparing with
	// our paths would clear a Windows agent every time and refuse nothing.
	if why := s.looseInCategoryDir("heracles", `C:\Hydra\movies`); why == "" {
		t.Error("a payload rooted at the agent's own category directory was allowed to move")
	}
	if why := s.looseInCategoryDir("heracles", "/config/tr-data/movies"); why != "" {
		t.Errorf("the agent was measured against this host's paths: %s", why)
	}
}
