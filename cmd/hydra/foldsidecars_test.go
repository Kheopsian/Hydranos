package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
)

const foldDoc = `[daemon]
data_dir = "%DIR%"

[race]
listen_port = 16171
max_connections = 4000

[hoard]
listen_port = 16172
max_connections = 12000
`

func foldFixture(t *testing.T, extras []config.EngineConfig) *config.HydraConfig {
	t.Helper()
	dir := t.TempDir()
	path := filepath.Join(dir, "default.toml")
	if err := os.WriteFile(path, []byte(strings.ReplaceAll(foldDoc, "%DIR%", dir)), 0644); err != nil {
		t.Fatal(err)
	}
	if extras != nil {
		b, _ := json.Marshal(extras)
		if err := os.WriteFile(filepath.Join(dir, "engines.json"), b, 0644); err != nil {
			t.Fatal(err)
		}
	}
	cfg, err := config.Reload(path)
	if err != nil {
		t.Fatal(err)
	}
	return cfg
}

// The fold was written, tested and never called: engines.json stayed the live
// source and the [[agent]] array stayed a plan. Wiring it is the point, so the
// test is that a boot actually moves them.
func TestBootFoldsTheSidecarIntoTheAgentArray(t *testing.T) {
	cfg := foldFixture(t, []config.EngineConfig{
		{ID: "vpn7", Role: "hoard", SessionConfig: config.SessionConfig{ListenPort: 26991, BindInterface: "wg7"}},
	})
	dir := cfg.Daemon.DataDir

	out := foldSidecars(cfg)

	if len(out.Agents) != 1 || out.Agents[0].Name != "local-vpn7" {
		t.Fatalf("[[agent]] entries = %+v, want one local-vpn7", out.Agents)
	}
	if _, err := os.Stat(filepath.Join(dir, "engines.json")); !os.IsNotExist(err) {
		t.Errorf("engines.json is still there: two sources again")
	}
	if _, err := os.Stat(filepath.Join(dir, "engines.json.migrated")); err != nil {
		t.Errorf("the sidecar was deleted rather than renamed aside: %v", err)
	}
	engines, err := out.ResolveEngines()
	if err != nil {
		t.Fatal(err)
	}
	var got *config.EngineConfig
	for i := range engines {
		if engines[i].ID == "vpn7" {
			got = &engines[i]
		}
	}
	if got == nil {
		t.Fatal("the migrated entry does not resolve as an engine: the fold is cosmetic")
	}
	if got.ListenPort != 26991 || got.BindInterface != "wg7" {
		t.Errorf("engine = port %d iface %q, want 26991/wg7", got.ListenPort, got.BindInterface)
	}
	// And it inherits the profile rather than running on zeroes.
	if got.MaxConnections != 12000 {
		t.Errorf("max_connections = %d, want the hoard profile's 12000", got.MaxConnections)
	}
}

// Nothing to fold must not touch the file. A boot that rewrites its own config
// every time is how comments and ordering disappear.
func TestBootWithNoSidecarLeavesTheConfigAlone(t *testing.T) {
	cfg := foldFixture(t, nil)
	before, _ := os.ReadFile(cfg.SourcePath)

	foldSidecars(cfg)

	after, _ := os.ReadFile(cfg.SourcePath)
	if string(before) != string(after) {
		t.Errorf("the config was rewritten with nothing to migrate:\n%s", after)
	}
	if _, err := os.Stat(cfg.SourcePath + ".bak-migrate"); err == nil {
		t.Errorf("a backup was written for a migration that had nothing to do")
	}
}
