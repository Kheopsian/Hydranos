package config

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	toml "github.com/pelletier/go-toml/v2"
)

func writeExtras(t *testing.T, dir string, engs []EngineConfig) {
	t.Helper()
	b, _ := json.Marshal(engs)
	if err := os.WriteFile(filepath.Join(dir, "engines.json"), b, 0644); err != nil {
		t.Fatal(err)
	}
}

const baseDoc = `[daemon]
data_dir = "/configs"

[race]
listen_port = 16171
`

// Two engines must land in two DIFFERENT entries. Writing the session as a
// nested [agent.session] header put the second engine's port inside the first
// agent's block -- one node silently configured with another's port.
func TestMigrationKeepsEachEngineInItsOwnEntry(t *testing.T) {
	dir := t.TempDir()
	writeExtras(t, dir, []EngineConfig{
		{ID: "vpn7", Role: "race", SessionConfig: SessionConfig{ListenPort: 26991, BindInterface: "wg7"}},
		{ID: "vpn8", Role: "race", SessionConfig: SessionConfig{ListenPort: 26992, BindInterface: "wg8"}},
	})
	out, done, err := MigrateSidecars(baseDoc, dir)
	if err != nil {
		t.Fatal(err)
	}
	if len(done) != 2 {
		t.Fatalf("%d migrations reported, want 2", len(done))
	}
	cfg := &HydraConfig{}
	if err := toml.Unmarshal([]byte(out), cfg); err != nil {
		t.Fatalf("migrated document does not parse: %v\n%s", err, out)
	}
	if len(cfg.Agents) != 2 {
		t.Fatalf("%d [[agent]] entries, want 2:\n%s", len(cfg.Agents), out)
	}
	byName := map[string]AgentConfig{}
	for _, a := range cfg.Agents {
		byName[a.Name] = a
	}
	for name, wantPort := range map[string]int{"local-vpn7": 26991, "local-vpn8": 26992} {
		a, ok := byName[name]
		if !ok {
			t.Fatalf("%s missing:\n%s", name, out)
		}
		if a.Session.ListenPort != wantPort {
			t.Errorf("%s listen_port = %d, want %d: a session landed in the wrong entry", name, a.Session.ListenPort, wantPort)
		}
	}
}

// The migrated entries must actually become engines, or the fold is cosmetic.
func TestMigratedEntriesResolveAsEngines(t *testing.T) {
	dir := t.TempDir()
	writeExtras(t, dir, []EngineConfig{
		{ID: "vpn7", Role: "race", SessionConfig: SessionConfig{ListenPort: 26991}},
	})
	out, _, err := MigrateSidecars(baseDoc, dir)
	if err != nil {
		t.Fatal(err)
	}
	cfg := &HydraConfig{}
	if err := toml.Unmarshal([]byte(out), cfg); err != nil {
		t.Fatal(err)
	}
	engines, err := cfg.ResolveEngines()
	if err != nil {
		t.Fatal(err)
	}
	found := false
	for _, e := range engines {
		if e.ID == "vpn7" && e.ListenPort == 26991 {
			found = true
		}
	}
	if !found {
		t.Errorf("the migrated engine is not resolved: %+v", engines)
	}
}

// Running twice must converge, not append. A half-failed migration is retried
// on the next boot, and a file that grows every time is unrecoverable.
func TestMigrationIsIdempotent(t *testing.T) {
	dir := t.TempDir()
	writeExtras(t, dir, []EngineConfig{{ID: "vpn7", Role: "race", SessionConfig: SessionConfig{ListenPort: 26991}}})
	once, _, err := MigrateSidecars(baseDoc, dir)
	if err != nil {
		t.Fatal(err)
	}
	twice, _, err := MigrateSidecars(once, dir)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Count(twice, "[[agent]]") != 1 {
		t.Errorf("%d entries after two runs, want 1:\n%s", strings.Count(twice, "[[agent]]"), twice)
	}
}

// No sidecar is a no-op, not an error: the vast majority of installs have none.
func TestMigrationWithoutSidecarsChangesNothing(t *testing.T) {
	out, done, err := MigrateSidecars(baseDoc, t.TempDir())
	if err != nil || len(done) != 0 || out != baseDoc {
		t.Errorf("empty migration changed something: err=%v done=%v", err, done)
	}
}
