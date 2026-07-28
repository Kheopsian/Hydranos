package state

import (
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"time"
)

// TorrentMeta holds the metadata needed to restore a torrent on startup.
type TorrentMeta struct {
	SavePath        string  `json:"save_path"`
	TorrentFilePath string  `json:"torrent_file,omitempty"`
	Category        string  `json:"category,omitempty"`
	AddedTime       float64 `json:"added_time,omitempty"`
	CompletedTime   float64 `json:"completed_time,omitempty"`
	TotalUploaded   int64   `json:"total_uploaded,omitempty"`
	TotalDownloaded int64   `json:"total_downloaded,omitempty"`
}

// State represents the persisted daemon state.
type State struct {
	Version     int                     `json:"version"`
	SavedAt     float64                 `json:"saved_at"`
	Race        map[string]*TorrentMeta `json:"race"`
	HoardActive map[string]*TorrentMeta `json:"hoard_active"`
}

// Manager handles state persistence to disk.
type Manager struct {
	stateDir  string
	stateFile string
}

// NewManager creates a new state manager, ensuring directories exist.
func NewManager(dataDir string) (*Manager, error) {
	stateFile := filepath.Join(dataDir, "state.json")

	if err := os.MkdirAll(dataDir, 0755); err != nil {
		return nil, fmt.Errorf("create state dir: %w", err)
	}

	return &Manager{
		stateDir:  dataDir,
		stateFile: stateFile,
	}, nil
}

// Save writes the full state to disk atomically (tmp + rename).
func (m *Manager) Save(raceTorrents, hoardActive map[string]*TorrentMeta) error {
	// Skip if state file is read-only (e.g., staging with imported prod state)
	if info, err := os.Stat(m.stateFile); err == nil {
		if info.Mode().Perm()&0200 == 0 {
			slog.Warn("State file is read-only, skipping save")
			return nil
		}
	}

	state := &State{
		Version:     1,
		SavedAt:     float64(time.Now().Unix()),
		Race:        raceTorrents,
		HoardActive: hoardActive,
	}

	data, err := json.MarshalIndent(state, "", "  ")
	if err != nil {
		return fmt.Errorf("marshal state: %w", err)
	}

	tmp := m.stateFile + ".tmp"
	if err := os.WriteFile(tmp, data, 0644); err != nil {
		return fmt.Errorf("write tmp state: %w", err)
	}
	if err := os.Rename(tmp, m.stateFile); err != nil {
		return fmt.Errorf("rename state: %w", err)
	}

	slog.Info("State saved",
		"race", len(raceTorrents),
		"hoard", len(hoardActive),
	)
	return nil
}

// Load reads the state from disk. Returns nil, nil if the file doesn't exist.
func (m *Manager) Load() (*State, error) {
	data, err := os.ReadFile(m.stateFile)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, fmt.Errorf("read state: %w", err)
	}

	var state State
	if err := json.Unmarshal(data, &state); err != nil {
		return nil, fmt.Errorf("unmarshal state: %w", err)
	}

	raceCount := len(state.Race)
	hoardCount := len(state.HoardActive)
	slog.Info("State loaded", "race", raceCount, "hoard", hoardCount)
	return &state, nil
}

// RemoveTorrent removes a torrent from the persisted state file.
func (m *Manager) RemoveTorrent(infoHash string) error {
	state, err := m.Load()
	if err != nil || state == nil {
		return err
	}

	removed := false
	if _, ok := state.Race[infoHash]; ok {
		delete(state.Race, infoHash)
		removed = true
	}
	if _, ok := state.HoardActive[infoHash]; ok {
		delete(state.HoardActive, infoHash)
		removed = true
	}

	if !removed {
		return nil
	}

	return m.Save(state.Race, state.HoardActive)
}
