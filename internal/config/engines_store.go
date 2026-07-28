package config

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

// enginesFile is the UI-managed list of EXTRA engines a node runs beyond its
// base [race]/[hoard] (Option A sharding). It is additive: managed from the
// Agents menu without hand-editing the TOML, applied on the next restart. Full
// TOML [[engine]] blocks (config.Engines) still override entirely — that path is
// for a pure agent node; this file is for adding shards to a monolith.
const enginesFile = "engines.json"

// LoadExtraEngines reads the UI-managed extra engines (empty if the file is
// absent). dataDir is Daemon.DataDir.
func LoadExtraEngines(dataDir string) ([]EngineConfig, error) {
	data, err := os.ReadFile(filepath.Join(dataDir, enginesFile))
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, fmt.Errorf("read %s: %w", enginesFile, err)
	}
	var engs []EngineConfig
	if err := json.Unmarshal(data, &engs); err != nil {
		return nil, fmt.Errorf("parse %s: %w", enginesFile, err)
	}
	return engs, nil
}

// ValidateEngines checks a merged engine set for unique ids, valid roles and
// unique listen ports (a clash would leave an engine dead at boot).
func ValidateEngines(engs []EngineConfig) error {
	seenID := make(map[string]bool, len(engs))
	seenPort := make(map[int]bool, len(engs))
	for i := range engs {
		e := &engs[i]
		if e.ID == "" {
			return fmt.Errorf("engine %d: empty id", i)
		}
		if e.Role != "race" && e.Role != "hoard" {
			return fmt.Errorf("engine %q: role must be race|hoard, got %q", e.ID, e.Role)
		}
		if seenID[e.ID] {
			return fmt.Errorf("duplicate engine id %q", e.ID)
		}
		seenID[e.ID] = true
		if e.ListenPort != 0 {
			if seenPort[e.ListenPort] {
				return fmt.Errorf("engine %q: listen_port %d already used", e.ID, e.ListenPort)
			}
			seenPort[e.ListenPort] = true
		}
	}
	return nil
}

// SaveExtraEngines writes the UI-managed extra engines atomically.
func SaveExtraEngines(dataDir string, engs []EngineConfig) error {
	data, err := json.MarshalIndent(engs, "", "  ")
	if err != nil {
		return err
	}
	tmp := filepath.Join(dataDir, enginesFile+".tmp")
	if err := os.WriteFile(tmp, data, 0644); err != nil {
		return err
	}
	return os.Rename(tmp, filepath.Join(dataDir, enginesFile))
}
